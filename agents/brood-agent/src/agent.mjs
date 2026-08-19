import { spawn } from "node:child_process";
import { chmod, mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { randomUUID } from "node:crypto";
import { NativeComputerUseClient } from "./computer-use.mjs";

const MAX_STEPS = 32;
const MAX_MODEL_ATTEMPTS = 4;
const MAX_TOOL_OUTPUT_BYTES = 1024 * 1024;
const CODEX_CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_TOKEN_URL = "https://auth.openai.com/oauth/token";
const CODEX_RESPONSES_URL = "https://chatgpt.com/backend-api/codex/responses";
const OPENAI_RESPONSES_URL = "https://api.openai.com/v1/responses";
const COMPUTER_USE_INSTRUCTIONS = "You are operating inside a Brood session. Computer use is provided as native computer_* tools scoped to this session. For any request to open, start, or launch a graphical application, call computer_launch before doing anything else. Never launch a graphical application through the shell: a shell process is not a Brood-managed application and cannot reliably connect to the session display. Do not claim that an application opened unless computer_launch returned visible=true. Use the other computer_* tools for subsequent interaction.";

const BASE_INSTRUCTIONS = `You are Brood's independent session agent. Work directly toward the user's requested outcome. You have a shell for files and commands in the session workspace and session-scoped computer-use tools for graphical applications. Inspect relevant state before changing it, preserve unrelated work, and verify changes with the real workload when practical. Keep user-facing prose concise.\n\n${COMPUTER_USE_INSTRUCTIONS}`;

function emit(type, fields = {}) {
  process.stdout.write(`${JSON.stringify({ type, ...fields })}\n`);
}

function bytePrefix(text, maximum = MAX_TOOL_OUTPUT_BYTES) {
  const bytes = Buffer.from(text);
  if (bytes.length <= maximum) return text;
  return `${bytes.subarray(0, maximum).toString("utf8")}\n[output truncated]`;
}

function textFromResponse(output) {
  return output
    .filter((item) => item?.type === "message")
    .flatMap((item) => item.content ?? [])
    .filter((item) => item?.type === "output_text")
    .map((item) => item.text ?? "")
    .join("");
}

function reasoningFromResponse(output) {
  return output
    .filter((item) => item?.type === "reasoning")
    .flatMap((item) => item.summary ?? [])
    .map((item) => item?.text ?? "")
    .join("\n");
}

function functionCalls(output) {
  return output.filter(
    (item) => item?.type === "function_call" && item.call_id && item.name,
  );
}

function addUsage(total, usage = {}) {
  total.requests += 1;
  total.input_tokens += Number(usage.input_tokens) || 0;
  total.cached_input_tokens += Number(usage.input_tokens_details?.cached_tokens) || 0;
  total.output_tokens += Number(usage.output_tokens) || 0;
  total.reasoning_tokens += Number(usage.output_tokens_details?.reasoning_tokens) || 0;
}

export function parseSsePayloads(text) {
  const payloads = [];
  for (const block of text.replaceAll("\r\n", "\n").split("\n\n")) {
    const data = block
      .split("\n")
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).trimStart())
      .join("\n");
    if (!data || data === "[DONE]") continue;
    payloads.push(JSON.parse(data));
  }
  return payloads;
}

async function readSse(response) {
  if (!response.body) throw new Error("model response had no body");
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let pending = "";
  let completed;
  const output = [];
  while (true) {
    const { value, done } = await reader.read();
    pending += decoder.decode(value ?? new Uint8Array(), { stream: !done });
    const blocks = pending.replaceAll("\r\n", "\n").split("\n\n");
    pending = done ? "" : blocks.pop() ?? "";
    for (const event of parseSsePayloads(`${blocks.join("\n\n")}\n\n`)) {
      if (event.type === "response.output_item.done" && event.item) output.push(event.item);
      if (event.type === "response.completed") completed = event.response;
      if (event.type === "response.failed") {
        throw new Error(event.response?.error?.message ?? "model response failed");
      }
      if (event.type === "error") {
        throw new Error(event.message ?? event.error?.message ?? "model stream failed");
      }
    }
    if (done) break;
  }
  if (!completed) {
    for (const event of parseSsePayloads(`${pending}\n\n`)) {
      if (event.type === "response.completed") completed = event.response;
    }
  }
  if (!completed) throw new Error("model stream ended without response.completed");
  if (!Array.isArray(completed.output) || completed.output.length === 0) {
    completed.output = output;
  }
  return completed;
}

function retryableModelResponse(status, text) {
  return status === 408
    || status === 429
    || status >= 500
    || (status === 400 && text.includes("prompt_cache_retention is not supported"));
}

function retryDelay(response, attempt, baseMs) {
  const retryAfter = response.headers.get("retry-after");
  if (retryAfter) {
    const seconds = Number(retryAfter);
    if (Number.isFinite(seconds)) return Math.max(0, seconds * 1000);
    const date = Date.parse(retryAfter);
    if (Number.isFinite(date)) return Math.max(0, date - Date.now());
  }
  return baseMs * 2 ** (attempt - 1);
}

async function waitForRetry(milliseconds, signal) {
  if (milliseconds <= 0) return;
  await new Promise((resolve, reject) => {
    const finish = () => {
      signal?.removeEventListener("abort", abort);
      resolve();
    };
    const timer = setTimeout(finish, milliseconds);
    const abort = () => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", abort);
      reject(signal.reason ?? new Error("turn cancelled"));
    };
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
  });
}

async function requestModel(fetchImpl, backend, body, signal, retryBaseMs) {
  for (let attempt = 1; attempt <= MAX_MODEL_ATTEMPTS; attempt += 1) {
    let response;
    try {
      response = await fetchImpl(backend.url, {
        method: "POST",
        headers: backend.headers,
        body: JSON.stringify(body),
        signal,
      });
    } catch (error) {
      if (signal?.aborted || attempt === MAX_MODEL_ATTEMPTS) throw error;
      emit("status", { text: `Model connection failed; retrying (${attempt}/${MAX_MODEL_ATTEMPTS})` });
      await waitForRetry(retryBaseMs * 2 ** (attempt - 1), signal);
      continue;
    }
    if (response.ok) return readSse(response);
    const text = bytePrefix(await response.text(), 4000);
    if (!retryableModelResponse(response.status, text) || attempt === MAX_MODEL_ATTEMPTS) {
      throw new Error(`model request failed (${response.status}): ${text}`);
    }
    emit("status", {
      text: `Model request failed (${response.status}); retrying (${attempt}/${MAX_MODEL_ATTEMPTS})`,
    });
    await waitForRetry(retryDelay(response, attempt, retryBaseMs), signal);
  }
  throw new Error("model request exhausted retries");
}

function decodeJwtPayload(token) {
  const payload = token.split(".")[1];
  if (!payload) throw new Error("access token is not a JWT");
  return JSON.parse(Buffer.from(payload, "base64url").toString("utf8"));
}

function codexAccountId(token, explicit) {
  if (explicit) return explicit;
  const payload = decodeJwtPayload(token);
  const account = payload?.["https://api.openai.com/auth"]?.chatgpt_account_id;
  if (!account) throw new Error("ChatGPT access token has no account id");
  return account;
}

function tokenExpiresSoon(token) {
  try {
    const expires = Number(decodeJwtPayload(token).exp) * 1000;
    return !Number.isFinite(expires) || expires < Date.now() + 60_000;
  } catch {
    return false;
  }
}

function broodAuthPath(env = process.env) {
  if (env.BROOD_AGENT_AUTH) return env.BROOD_AGENT_AUTH;
  const config = env.XDG_CONFIG_HOME || join(homedir(), ".config");
  return join(config, "brood", "agent-auth.json");
}

function codexAuthPath(env = process.env) {
  if (env.BROOD_AGENT_CODEX_AUTH) return env.BROOD_AGENT_CODEX_AUTH;
  return join(env.HOME || homedir(), ".codex", "auth.json");
}

async function readJson(path) {
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    if (error?.code === "ENOENT") return undefined;
    throw error;
  }
}

async function saveBroodAuth(auth, path = broodAuthPath()) {
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  await chmod(dirname(path), 0o700);
  const temporary = `${path}.${process.pid}.tmp`;
  await writeFile(temporary, `${JSON.stringify(auth, null, 2)}\n`, { mode: 0o600 });
  await chmod(temporary, 0o600);
  await rename(temporary, path);
}

async function refreshCodexAuth(auth, path, fetchImpl = fetch) {
  const response = await fetchImpl(CODEX_TOKEN_URL, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "refresh_token",
      refresh_token: auth.refresh_token,
      client_id: CODEX_CLIENT_ID,
    }),
  });
  if (!response.ok) {
    throw new Error(`refresh Brood agent login: HTTP ${response.status}`);
  }
  const tokens = await response.json();
  if (!tokens.access_token || !tokens.refresh_token) {
    throw new Error("refresh Brood agent login: token response was incomplete");
  }
  const refreshed = {
    provider: "openai-codex",
    access_token: tokens.access_token,
    refresh_token: tokens.refresh_token,
    account_id: codexAccountId(
      tokens.access_token,
      tokens.account_id ?? auth.account_id,
    ),
  };
  await saveBroodAuth(refreshed, path);
  return refreshed;
}

export async function resolveModelBackend(env = process.env, fetchImpl = fetch) {
  if (env.OPENAI_API_KEY) {
    return {
      kind: "openai-responses",
      url: env.BROOD_AGENT_MODEL_URL || OPENAI_RESPONSES_URL,
      headers: {
        authorization: `Bearer ${env.OPENAI_API_KEY}`,
        "content-type": "application/json",
        accept: "text/event-stream",
      },
    };
  }

  const authPath = broodAuthPath(env);
  let auth = await readJson(authPath);
  if (!auth) {
    const imported = await readJson(codexAuthPath(env));
    if (imported?.tokens?.access_token && imported.tokens.refresh_token) {
      auth = {
        provider: "openai-codex",
        access_token: imported.tokens.access_token,
        refresh_token: imported.tokens.refresh_token,
        account_id: imported.tokens.account_id,
      };
      await saveBroodAuth(auth, authPath);
    }
  }
  if (!auth?.access_token) {
    throw new Error(
      "No Brood agent model login. Set OPENAI_API_KEY or provide Brood agent OAuth credentials.",
    );
  }
  if (tokenExpiresSoon(auth.access_token)) {
    if (!auth.refresh_token) throw new Error("Brood agent model login expired");
    auth = await refreshCodexAuth(auth, authPath, fetchImpl);
  }
  const account = codexAccountId(auth.access_token, auth.account_id);
  return {
    kind: "openai-codex",
    url: env.BROOD_AGENT_MODEL_URL || CODEX_RESPONSES_URL,
    headers: {
      authorization: `Bearer ${auth.access_token}`,
      "chatgpt-account-id": account,
      originator: "brood",
      "user-agent": "brood-agent/0.1.0",
      "openai-beta": "responses=experimental",
      "content-type": "application/json",
      accept: "text/event-stream",
    },
  };
}

const SHELL_TOOL = {
  type: "function",
  name: "shell",
  description: "Run one shell command in the session workspace and return its combined output and exit status.",
  parameters: {
    type: "object",
    properties: {
      command: { type: "string", description: "Shell command to run." },
      timeout_ms: { type: "integer", minimum: 1, maximum: 120000 },
    },
    required: ["command"],
    additionalProperties: false,
  },
  strict: false,
};

async function runShell(args, signal) {
  const timeout = Math.min(Math.max(Number(args.timeout_ms) || 30_000, 1), 120_000);
  return new Promise((resolve) => {
    const child = spawn("bash", ["-lc", String(args.command ?? "")], {
      cwd: process.cwd(),
      env: process.env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let output = "";
    const collect = (chunk) => {
      if (Buffer.byteLength(output) < MAX_TOOL_OUTPUT_BYTES) output += chunk.toString("utf8");
    };
    child.stdout.on("data", collect);
    child.stderr.on("data", collect);
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGTERM");
    }, timeout);
    const abort = () => child.kill("SIGTERM");
    signal?.addEventListener("abort", abort, { once: true });
    child.once("exit", (code, killedBy) => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", abort);
      resolve({
        content: bytePrefix(`${output}\n[exit ${code ?? killedBy ?? "unknown"}${timedOut ? "; timed out" : ""}]`),
        isError: timedOut || code !== 0,
      });
    });
  });
}

function parseArguments(call) {
  try {
    return JSON.parse(call.arguments || "{}");
  } catch (error) {
    throw new Error(`invalid arguments for ${call.name}: ${error.message}`);
  }
}

async function callTool(call, computerUse, signal) {
  const args = parseArguments(call);
  emit("tool_start", { tool: call.name, arguments: args });
  try {
    let result;
    if (call.name === "shell") {
      result = await runShell(args, signal);
      result.output = [{ type: "input_text", text: result.content }];
      result.text = result.content;
    } else if (call.name.startsWith("computer_")) {
      result = await computerUse.call(call.name, args, call.call_id);
    } else {
      throw new Error(`unknown tool ${call.name}`);
    }
    emit("tool_end", {
      tool: call.name,
      text: bytePrefix(result.text, 4000),
      is_error: result.isError,
    });
    return { type: "function_call_output", call_id: call.call_id, output: result.output };
  } catch (error) {
    const text = error instanceof Error ? error.message : String(error);
    emit("tool_end", { tool: call.name, text, is_error: true });
    return {
      type: "function_call_output",
      call_id: call.call_id,
      output: [{ type: "input_text", text: `Tool failed: ${text}` }],
    };
  }
}

function priorMessages(history) {
  return (history ?? []).flatMap((turn) => [
    { role: "user", content: [{ type: "input_text", text: turn.user }] },
    { role: "assistant", content: [{ type: "output_text", text: turn.assistant }] },
  ]);
}

async function imageInput(image) {
  const bytes = await readFile(image.path);
  return {
    type: "input_image",
    image_url: `data:${image.mime_type};base64,${bytes.toString("base64")}`,
  };
}

export async function runAgent(request, options = {}) {
  const fetchImpl = options.fetchImpl ?? fetch;
  const signal = options.signal;
  const modelRetryBaseMs = options.modelRetryBaseMs ?? 500;
  const backend = options.backend ?? (await resolveModelBackend(process.env, fetchImpl));
  const computerUse = options.computerUse ?? new NativeComputerUseClient({
    session: request.session_id,
    principal: request.agent,
  });
  try {
    const tools = [SHELL_TOOL, ...computerUse.tools()];
    const currentUserContent = [{ type: "input_text", text: request.prompt }];
    for (const image of request.images ?? []) currentUserContent.push(await imageInput(image));
    const input = [
      ...priorMessages(request.history),
      { role: "user", content: currentUserContent },
    ];
    emit("status", {
      text: `Starting Brood agent · model=${request.model} · reasoning=${request.reasoning}`,
    });

    let finalText = "";
    const usage = {
      requests: 0,
      input_tokens: 0,
      cached_input_tokens: 0,
      output_tokens: 0,
      reasoning_tokens: 0,
    };
    for (let step = 1; step <= MAX_STEPS; step += 1) {
      if (signal?.aborted) throw new Error("turn cancelled");
      const model = !request.model || request.model === "default" ? "gpt-5.4" : request.model;
      const body = {
        model,
        store: false,
        stream: true,
        instructions: BASE_INSTRUCTIONS,
        input,
        tools,
        tool_choice: "auto",
        parallel_tool_calls: false,
        reasoning: { effort: request.reasoning || "medium", summary: "auto" },
        text: { verbosity: "low" },
        include: ["reasoning.encrypted_content"],
        prompt_cache_key: `${request.agent}-${request.session_id}`,
      };
      const completed = await requestModel(
        fetchImpl,
        backend,
        body,
        signal,
        modelRetryBaseMs,
      );
      addUsage(usage, completed.usage);
      const output = completed.output ?? [];
      const reasoning = reasoningFromResponse(output);
      if (reasoning) emit("reasoning", { text: reasoning });
      const text = textFromResponse(output);
      if (text) {
        finalText = text;
        emit("assistant", { text });
      }
      input.push(...output);
      const calls = functionCalls(output);
      if (calls.length === 0) {
        emit("status", { text: "Turn complete" });
        const history = [...(request.history ?? []), { user: request.prompt, assistant: finalText }]
          .slice(-24);
        emit("result", { history, usage });
        return history;
      }
      for (const call of calls) input.push(await callTool(call, computerUse, signal));
    }
    throw new Error(`agent exceeded ${MAX_STEPS} model steps`);
  } finally {
    if (!options.computerUse) await computerUse.close();
  }
}

export function createAbortController() {
  const controller = new AbortController();
  for (const event of ["SIGINT", "SIGTERM"]) {
    process.once(event, () => controller.abort(new Error("turn cancelled")));
  }
  return controller;
}

export function requestId() {
  return randomUUID();
}
