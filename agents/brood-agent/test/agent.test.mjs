import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  mcpCallArguments,
  mcpToolDefinition,
  parseSsePayloads,
  resolveModelBackend,
  runAgent,
} from "../src/agent.mjs";

function fakeJwt(accountId, expiresAtSeconds) {
  const encode = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
  return `${encode({ alg: "none" })}.${encode({
    exp: expiresAtSeconds,
    "https://api.openai.com/auth": { chatgpt_account_id: accountId },
  })}.test-signature`;
}

async function temporaryAuthHome(t) {
  const root = await mkdtemp(join(tmpdir(), "brood-agent-auth-test."));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, ".codex"), { recursive: true });
  return root;
}

test("parses response SSE blocks and ignores DONE", () => {
  assert.deepEqual(
    parseSsePayloads(
      'event: response.completed\ndata: {"type":"response.completed","response":{"output":[]}}\n\ndata: [DONE]\n\n',
    ),
    [{ type: "response.completed", response: { output: [] } }],
  );
});

test("API key selects the public Responses backend", async () => {
  const backend = await resolveModelBackend({ OPENAI_API_KEY: "test-key" });
  assert.equal(backend.kind, "openai-responses");
  assert.equal(backend.url, "https://api.openai.com/v1/responses");
  assert.equal(backend.headers.authorization, "Bearer test-key");
});

test("imports a valid Codex login once into a private Brood auth file", async (t) => {
  const home = await temporaryAuthHome(t);
  const accessToken = fakeJwt("account-imported", Math.floor(Date.now() / 1000) + 3600);
  await writeFile(
    join(home, ".codex", "auth.json"),
    JSON.stringify({
      auth_mode: "chatgpt",
      tokens: {
        access_token: accessToken,
        refresh_token: "refresh-imported",
        account_id: "account-imported",
      },
    }),
  );
  const env = { HOME: home, XDG_CONFIG_HOME: join(home, "config") };
  const backend = await resolveModelBackend(env, () => {
    throw new Error("a valid imported login must not refresh");
  });
  const path = join(home, "config", "brood", "agent-auth.json");
  const saved = JSON.parse(await readFile(path, "utf8"));

  assert.equal(backend.kind, "openai-codex");
  assert.equal(backend.headers.authorization, `Bearer ${accessToken}`);
  assert.deepEqual(saved, {
    provider: "openai-codex",
    access_token: accessToken,
    refresh_token: "refresh-imported",
    account_id: "account-imported",
  });
  assert.equal((await stat(join(home, "config", "brood"))).mode & 0o777, 0o700);
  assert.equal((await stat(path)).mode & 0o777, 0o600);
});

test("refreshes an expired imported login into the selected Brood auth file", async (t) => {
  const home = await temporaryAuthHome(t);
  const expired = fakeJwt("account-refresh", Math.floor(Date.now() / 1000) - 60);
  const refreshed = fakeJwt("account-refresh", Math.floor(Date.now() / 1000) + 3600);
  await writeFile(
    join(home, ".codex", "auth.json"),
    JSON.stringify({
      tokens: {
        access_token: expired,
        refresh_token: "old-refresh",
        account_id: "account-refresh",
      },
    }),
  );
  const authPath = join(home, "private", "brood-oauth.json");
  const backend = await resolveModelBackend(
    { HOME: home, BROOD_AGENT_AUTH: authPath },
    async (url, request) => {
      assert.equal(url, "https://auth.openai.com/oauth/token");
      assert.equal(request.method, "POST");
      assert.equal(request.body.get("grant_type"), "refresh_token");
      assert.equal(request.body.get("refresh_token"), "old-refresh");
      return new Response(JSON.stringify({
        access_token: refreshed,
        refresh_token: "new-refresh",
      }), { status: 200, headers: { "content-type": "application/json" } });
    },
  );
  const saved = JSON.parse(await readFile(authPath, "utf8"));

  assert.equal(backend.headers.authorization, `Bearer ${refreshed}`);
  assert.equal(saved.access_token, refreshed);
  assert.equal(saved.refresh_token, "new-refresh");
  assert.equal((await stat(authPath)).mode & 0o777, 0o600);
});

test("computer actions hide and restore the MCP transport envelope", () => {
  const directActionSchema = {
    type: "object",
    required: ["task", "actions"],
    properties: { task: { type: "string" }, actions: { type: "array" } },
  };
  const definition = mcpToolDefinition({
    name: "computer_action",
    inputSchema: {
      type: "object",
      required: ["tool_call_id", "arguments"],
      properties: {
        tool_call_id: { type: "string" },
        arguments: directActionSchema,
      },
    },
  });

  assert.equal(definition.name, "brood_computer_use__computer_action");
  assert.deepEqual(definition.parameters, directActionSchema);
  assert.deepEqual(
    mcpCallArguments("computer_action", "model-call-7", {
      task: "task-1",
      actions: [{ type: "screenshot" }],
    }),
    {
      tool_call_id: "model-call-7",
      arguments: {
        task: "task-1",
        actions: [{ type: "screenshot" }],
        call_id: "model-call-7",
      },
    },
  );
});

test("completed turns report aggregate model usage", async () => {
  const writes = [];
  const originalWrite = process.stdout.write;
  process.stdout.write = (chunk) => {
    writes.push(String(chunk));
    return true;
  };
  try {
    await runAgent(
      {
        session_id: "usage-test",
        agent: "agent-test",
        prompt: "finish",
        model: "test-model",
        reasoning: "medium",
        history: [],
      },
      {
        backend: { url: "https://example.invalid", headers: {} },
        mcp: { async tools() { return []; } },
        fetchImpl: async () => new Response(
          'data: {"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],"usage":{"input_tokens":11,"input_tokens_details":{"cached_tokens":3},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":5}}}}\n\ndata: [DONE]\n\n',
          { status: 200, headers: { "content-type": "text/event-stream" } },
        ),
      },
    );
  } finally {
    process.stdout.write = originalWrite;
  }
  const events = writes.join("").trim().split("\n").map(JSON.parse);
  assert.deepEqual(events.at(-1).usage, {
    requests: 1,
    input_tokens: 11,
    cached_input_tokens: 3,
    output_tokens: 7,
    reasoning_tokens: 5,
  });
});

test("transient model failures retry without losing the turn", async () => {
  const writes = [];
  const originalWrite = process.stdout.write;
  process.stdout.write = (chunk) => {
    writes.push(String(chunk));
    return true;
  };
  let requests = 0;
  try {
    await runAgent(
      {
        session_id: "retry-test",
        agent: "agent-test",
        prompt: "finish",
        model: "test-model",
        reasoning: "medium",
        history: [],
      },
      {
        backend: { url: "https://example.invalid", headers: {} },
        mcp: { async tools() { return []; } },
        modelRetryBaseMs: 0,
        fetchImpl: async () => {
          requests += 1;
          if (requests === 1) return new Response("temporarily unavailable", { status: 503 });
          if (requests === 2) {
            return new Response(
              '{"error":{"message":"prompt_cache_retention is not supported on this model"}}',
              { status: 400 },
            );
          }
          return new Response(
            'data: {"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],"usage":{"input_tokens":2,"output_tokens":1}}}\n\ndata: [DONE]\n\n',
            { status: 200, headers: { "content-type": "text/event-stream" } },
          );
        },
      },
    );
  } finally {
    process.stdout.write = originalWrite;
  }
  const events = writes.join("").trim().split("\n").map(JSON.parse);
  assert.equal(requests, 3);
  assert(events.some((event) => event.type === "status" && event.text.includes("503")));
  assert(events.some((event) => event.type === "status" && event.text.includes("400")));
  assert.equal(events.at(-1).history[0].assistant, "done");
});
