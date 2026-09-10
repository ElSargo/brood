import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { createServer } from "node:net";
import { join } from "node:path";
import test from "node:test";

import {
  parseSsePayloads,
  resolveModelBackend,
  runAgent,
  runExecCommand,
} from "../src/agent.mjs";
import {
  NativeComputerUseClient,
  computerUseTools,
  nativeToolResult,
  requestNativeTool,
} from "../src/computer-use.mjs";

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

test("exec_command returns structured stdout, stderr, status, and workdir", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "brood-agent-exec-test."));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "nested"));
  const result = await runExecCommand({
    command: "printf output; printf problem >&2",
    workdir: "nested",
  }, undefined, root);
  const output = JSON.parse(result.content);
  assert.equal(output.stdout, "output");
  assert.equal(output.stderr, "problem");
  assert.equal(output.exit_code, 0);
  assert.equal(output.workdir, join(root, "nested"));
  assert.equal(output.truncated, false);
  assert.equal(result.isError, false);
});

test("exec_command confines workdirs and bounds noisy output with useful tail", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "brood-agent-exec-bounds."));
  const outside = await mkdtemp(join(tmpdir(), "brood-agent-exec-outside."));
  t.after(() => rm(root, { recursive: true, force: true }));
  t.after(() => rm(outside, { recursive: true, force: true }));
  await assert.rejects(
    runExecCommand({ command: "pwd", workdir: ".." }, undefined, root),
    /inside the session workspace/,
  );
  await symlink(outside, join(root, "escape"));
  await assert.rejects(
    runExecCommand({ command: "pwd", workdir: "escape" }, undefined, root),
    /inside the session workspace/,
  );
  const exact = await runExecCommand({
    command: "head -c 1024 /dev/zero | tr '\\0' x",
    max_output_bytes: 1024,
  }, undefined, root);
  const exactOutput = JSON.parse(exact.content);
  assert.equal(exactOutput.stdout, "x".repeat(1024));
  assert.equal(exactOutput.truncated, false);
  const result = await runExecCommand({
    command: "printf begin; head -c 3000 /dev/zero | tr '\\0' x; printf end",
    max_output_bytes: 1024,
  }, undefined, root);
  const output = JSON.parse(result.content);
  assert.equal(output.truncated, true);
  assert(output.stdout.startsWith("begin"));
  assert(output.stdout.endsWith("end"));
  assert(output.stdout.includes("output truncated"));
});

test("exec_command terminates a timed out process group", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "brood-agent-exec-timeout."));
  t.after(() => rm(root, { recursive: true, force: true }));
  const result = await runExecCommand({ command: "sleep 10", timeout_ms: 20 }, undefined, root);
  const output = JSON.parse(result.content);
  assert.equal(output.timed_out, true);
  assert.equal(result.isError, true);
  assert(output.duration_ms < 2000);
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

test("computer actions are native model tools with a hidden host envelope", async () => {
  const definition = computerUseTools(7).find((tool) => tool.name === "computer_action");
  assert.equal(definition.name, "computer_action");
  assert(definition.description.includes("authoritative evidence"));
  assert(definition.description.includes("visibly present"));
  assert(definition.parameters.required.includes("task"));
  assert(!definition.parameters.properties.tool_call_id);

  const commands = [];
  const client = new NativeComputerUseClient({ socketPath: "/unused", session: 7, principal: "agent-1" });
  client.command = async (command) => {
    commands.push(command);
    return "{}";
  };
  await client.call("computer_start", { task: "task-1" }, "start-call");
  await client.call("computer_action", {
    protocol_version: 1,
    task: "task-1",
    lease_generation: 1,
    expected_sequence: 1,
    actions: [{ type: "screenshot" }],
  }, "model-call-7");
  const envelope = JSON.parse(commands[1][2]);
  assert.equal(commands[1][1], "native-call");
  assert.equal(envelope.tool_call_id, "model-call-7");
  assert.equal(envelope.arguments.target_session, 7);
  assert.equal(envelope.arguments.call_id, "model-call-7");
});

test("computer client hides lease, action sequence, and observation cursors", async () => {
  const commands = [];
  const replies = [
    '{"generation":7}',
    '{"sequence":1,"observation":{"sequence":3}}',
    '{"sequence":2}',
    '{"events":[],"observation":null}',
  ];
  const client = new NativeComputerUseClient({ socketPath: "/unused", session: 7, principal: "agent-1" });
  client.command = async (command) => {
    commands.push(command);
    return replies.shift();
  };
  await client.call("computer_start", { task: "task-auto" }, "start-auto");
  const actionResult = await client.call("computer_action", {
    task: "task-auto",
    actions: [{ type: "screenshot" }],
  }, "action-auto");
  await client.call("computer_exec_js", {
    task: "task-auto",
    source: "display(computer.screenshot())",
  }, "script-auto");
  await client.call("computer_observe", { task: "task-auto" }, "observe-auto");

  const action = JSON.parse(commands[1][2]).arguments;
  assert.equal(action.protocol_version, 1);
  assert.equal(action.lease_generation, 7);
  assert.equal(action.expected_sequence, 1);
  assert.equal(actionResult.summary, "Computer updated · sequence 1");
  const script = JSON.parse(commands[2][2]);
  assert.equal(script.lease_generation, 7);
  assert.equal(script.expected_sequence, 2);
  assert.deepEqual(commands[3], ["computer-use", "observe", "task-auto", "0", "3"]);
  const definition = computerUseTools(7).find((tool) => tool.name === "computer_exec_js");
  assert.deepEqual(definition.parameters.required, ["task", "source"]);
  assert(definition.description.includes("Prefer this"));
});

test("computer window discovery is session scoped and recommended before launch", async () => {
  const definitions = computerUseTools(7);
  const windows = definitions.find((tool) => tool.name === "computer_windows");
  const launch = definitions.find((tool) => tool.name === "computer_launch");
  assert(windows.description.includes("session 7"));
  assert(windows.description.includes("first"));
  assert(launch.description.includes("computer_windows first"));

  const commands = [];
  const client = new NativeComputerUseClient({ socketPath: "/unused", session: 7, principal: "agent-1" });
  client.command = async (command) => {
    commands.push(command);
    return '{"session":7,"windows":[]}';
  };
  const result = await client.call("computer_windows");
  assert.deepEqual(commands, [["computer-use", "windows", "7"]]);
  assert.equal(result.text, '{"session":7,"windows":[]}');
});

test("native computer results extract PNGs from model-visible text", () => {
  const result = nativeToolResult(JSON.stringify({
    sequence: 2,
    observation: { png: [137, 80, 78, 71, 13, 10, 26, 10, 1] },
  }));
  assert.equal(result.output[0].type, "input_text");
  assert(!result.output[0].text.includes("137,80,78,71"));
  assert.equal(result.output[1].type, "input_image");
});

test("native computer transport frames commands and returns daemon output", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "brood-native-tool-test."));
  const socketPath = join(root, "tool.sock");
  let received;
  const server = createServer((socket) => {
    let request = "";
    socket.on("data", (chunk) => { request += chunk.toString("utf8"); });
    socket.on("end", () => {
      received = JSON.parse(request.trim());
      socket.end(`${JSON.stringify({ protocol_version: 1, ok: true, output: "ready" })}\n`);
    });
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(socketPath, resolve);
  });
  t.after(async () => {
    await new Promise((resolve) => server.close(resolve));
    await rm(root, { recursive: true, force: true });
  });

  assert.equal(await requestNativeTool(socketPath, ["computer-use", "status"]), "ready");
  assert.deepEqual(received, { command: ["computer-use", "status"] });
});

test("native computer transport surfaces daemon errors", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "brood-native-tool-error-test."));
  const socketPath = join(root, "tool.sock");
  const server = createServer((socket) => {
    socket.on("data", () => {});
    socket.on("end", () => {
      socket.end(`${JSON.stringify({ protocol_version: 1, ok: false, error: "denied" })}\n`);
    });
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(socketPath, resolve);
  });
  t.after(async () => {
    await new Promise((resolve) => server.close(resolve));
    await rm(root, { recursive: true, force: true });
  });

  await assert.rejects(
    requestNativeTool(socketPath, ["computer-use", "status"]),
    /denied/,
  );
});

test("completed turns report aggregate model usage", async () => {
  const events = [];
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
      emit: (type, fields) => events.push({ type, ...fields }),
      persist: false,
      config: {},
      backend: { url: "https://example.invalid", headers: {} },
      computerUse: { tools() { return []; } },
      fetchImpl: async () => new Response(
        'data: {"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],"usage":{"input_tokens":11,"input_tokens_details":{"cached_tokens":3},"output_tokens":7,"output_tokens_details":{"reasoning_tokens":5}}}}\n\ndata: [DONE]\n\n',
        { status: 200, headers: { "content-type": "text/event-stream" } },
      ),
    },
  );
  assert.deepEqual(events.at(-1).usage, {
    requests: 1,
    input_tokens: 11,
    cached_input_tokens: 3,
    output_tokens: 7,
    reasoning_tokens: 5,
  });
});

test("transient model failures retry without losing the turn", async () => {
  const events = [];
  let requests = 0;
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
      emit: (type, fields) => events.push({ type, ...fields }),
      persist: false,
      config: {},
      backend: { url: "https://example.invalid", headers: {} },
      computerUse: { tools() { return []; } },
      modelRetryBaseMs: 0,
      fetchImpl: async () => {
        requests += 1;
        if (requests === 1) return new Response("temporarily unavailable", { status: 503 });
        if (requests === 2) {
          return new Response(
            '{"error":{"message":"rate limited"}}',
            { status: 429 },
          );
        }
        return new Response(
          'data: {"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],"usage":{"input_tokens":2,"output_tokens":1}}}\n\ndata: [DONE]\n\n',
          { status: 200, headers: { "content-type": "text/event-stream" } },
        );
      },
    },
  );
  assert.equal(requests, 3);
  assert.equal(events.filter((event) => event.type === "status" && event.text.includes("retrying")).length, 2);
  assert.equal(events.at(-1).history[0].assistant, "done");
});
