import assert from "node:assert/strict";
import test from "node:test";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, writeFile, mkdir, rm, stat, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import { runAgent } from "../src/agent.mjs";
import { validateConfig, resolveSettings, customBackend } from "../src/config.mjs";
import { ConversationStore, recoverInterruptedCalls } from "../src/storage.mjs";
import { readSse, requestModel } from "../src/transport.mjs";
import { CommandSessions, runExecCommand } from "../src/commands.mjs";
import { CodeRepl } from "../src/repl.mjs";
import { applyPatch } from "../src/patch.mjs";
import { resolveModelBackend } from "../src/auth.mjs";

const message = (text, phase = "final_answer") => ({ type: "message", role: "assistant", phase, content: [{ type: "output_text", text }] });
const call = (name, args, id = randomUUID()) => ({ type: "function_call", name, arguments: typeof args === "string" ? args : JSON.stringify(args), call_id: id });
const completed = output => ({ status: "completed", output, usage: { input_tokens: 10, output_tokens: 5 } });
const sse = output => new Response(`data: ${JSON.stringify({ type: "response.completed", response: completed(output) })}\n\n`, { headers: { "content-type": "text/event-stream" } });
async function temporary(t) {
  const root = await mkdtemp(join(tmpdir(), "brood-reliability-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}
async function fixture(t, responder) {
  const root = await temporary(t);
  const workspace = join(root, "workspace");
  await mkdir(workspace);
  const requests = [];
  const server = createServer(async (request, response) => {
    try {
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks));
      requests.push({ path: request.url, body, headers: request.headers });
      await responder(body, response, request.url, requests.length);
    } catch (error) {
      response.writeHead(500); response.end(error.stack);
    }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const events = [];
  const config = {
    provider: "fixture", model: "fixture-model", max_retries: 0,
    providers: { fixture: { base_url: `http://127.0.0.1:${server.address().port}/v1`, env_key: "FIXTURE_KEY" } },
  };
  const options = { config, env: { FIXTURE_KEY: "fixture-secret" }, stateDirectory: join(root, "state"), workspace, computerUse: { tools: () => [] }, emit: (type, fields) => events.push({ type, ...fields }), modelRetryBaseMs: 0 };
  const send = (response, output) => { response.writeHead(200, { "content-type": "text/event-stream" }); response.end(`data: ${JSON.stringify({ type: "response.completed", response: completed(output) })}\n\n`); };
  return { root, workspace, requests, events, options, send };
}

test("provider profiles validate settings and never borrow credentials", () => {
  const config = { provider: "custom", providers: { custom: { base_url: "http://localhost:1234/v1", env_key: "CUSTOM_KEY", env_http_headers: { "x-api-key": "CUSTOM_HEADER" }, query_params: { version: "one" } } }, profiles: { quick: { model: "small", max_steps: 80 } } };
  const settings = resolveSettings(config, { profile: "quick", model: "default" });
  assert.equal(settings.model, "small");
  assert.equal(settings.max_steps, 80);
  assert.throws(() => customBackend(settings, { OPENAI_API_KEY: "wrong" }), /CUSTOM_KEY/);
  const backend = customBackend(settings, { CUSTOM_KEY: "right", CUSTOM_HEADER: "header" });
  assert.equal(backend.url, "http://localhost:1234/v1/responses?version=one");
  assert.equal(backend.headers.authorization, "Bearer right");
  assert.equal(backend.headers["x-api-key"], "header");
  assert.throws(() => validateConfig({ max_step: 1 }), /Unknown/);
  assert.throws(() => validateConfig({ providers: { x: { base_url: "file:///etc", env_key: "KEY" } } }), /HTTP/);
  assert.throws(() => resolveSettings({}, { profile: "missing" }), /Unknown agent profile/);
  assert.throws(() => resolveSettings({ context_window: 10000, compact_threshold: 10000 }), /below/);
});

test("SSE handles arbitrary UTF-8 and CRLF boundaries and terminal event without EOF", async () => {
  const payload = `data: ${JSON.stringify({ type: "response.completed", response: completed([message("héλlo")]) })}\r\n\r\n`;
  const bytes = Buffer.from(payload);
  let cancelled = false;
  const stream = new ReadableStream({ start(controller) { for (const byte of bytes) controller.enqueue(Uint8Array.of(byte)); }, cancel() { cancelled = true; } });
  const response = await readSse(new Response(stream), { idleTimeoutMs: 100 });
  assert.equal(response.output[0].content[0].text, "héλlo");
  assert(cancelled);
  const unterminated = await readSse(new Response(payload.trimEnd()));
  assert.equal(unterminated.status, "completed");
});

test("stream stalls and incomplete responses fail; permanent request errors are not retried", async () => {
  await assert.rejects(readSse(new Response(new ReadableStream({ start() {} })), { idleTimeoutMs: 10 }), /stalled/);
  await assert.rejects(readSse(new Response('data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}\n\n')), /max_output_tokens/);
  let attempts = 0;
  await assert.rejects(requestModel(async () => { attempts++; return new Response('{"error":{"message":"secret-key invalid"}}', { status: 401 }); }, { url: "https://example.invalid", headers: { authorization: "Bearer secret-key" } }, {}, { retryBaseMs: 0 }), error => {
    assert(!error.message.includes("secret-key")); return /401/.test(error.message);
  });
  assert.equal(attempts, 1);
});

test("interrupted streams retry without executing partial tool calls", async () => {
  let attempt = 0;
  const response = await requestModel(async () => ++attempt === 1
    ? new Response(`data: ${JSON.stringify({ type: "response.output_item.done", item: call("exec_command", { command: "never execute" }) })}\n\n`)
    : sse([message("recovered")]), { url: "http://fixture", headers: {} }, {}, { retryBaseMs: 0 });
  assert.equal(attempt, 2);
  assert.equal(response.output.length, 1);
  assert.equal(response.output[0].type, "message");
});

test("command sessions yield, accept stdin, and drain final bytes", async t => {
  const root = await temporary(t);
  const commands = new CommandSessions({ workspace: root, artifactDirectory: join(root, "logs"), outputBytes: 1024 });
  t.after(() => commands.close());
  let output = JSON.parse((await commands.exec({ command: "read value; printf 'received:%s' \"$value\"; printf tail >&2", yield_time_ms: 0 })).content);
  assert(output.session_id);
  output = JSON.parse((await commands.write({ session_id: output.session_id, chars: "hello\n", yield_time_ms: 1000 })).content);
  assert.equal(output.stdout, "received:hello");
  assert.equal(output.stderr, "tail");
  assert.equal(output.exit_code, 0);
  assert.equal(await readFile(output.output_files.stdout, "utf8"), output.stdout);
  assert.equal((await stat(output.output_files.stdout)).mode & 0o777, 0o600);
});

test("pre-cancelled commands have no effects and timeout kills descendants", async t => {
  const root = await temporary(t);
  const controller = new AbortController(); controller.abort(new Error("cancelled"));
  await assert.rejects(runExecCommand({ command: "touch forbidden" }, controller.signal, root), /cancelled/);
  await assert.rejects(stat(join(root, "forbidden")), { code: "ENOENT" });
  const result = await runExecCommand({ command: "touch started; (trap '' TERM; sleep 2; touch leaked) & wait", timeout_ms: 500 }, undefined, root);
  assert(JSON.parse(result.content).timed_out);
  await stat(join(root, "started"));
  await new Promise(resolve => setTimeout(resolve, 1800));
  await assert.rejects(stat(join(root, "leaked")), { code: "ENOENT" });
});

test("patch validates the whole change, supports move, and rejects ambiguous context", async t => {
  const root = await temporary(t);
  await writeFile(join(root, "one"), "first\nsecond\n");
  await assert.rejects(applyPatch("*** Begin Patch\n*** Add File: new\n+created\n*** Update File: one\n@@\n-absent\n+new\n*** End Patch", root), /context not found/);
  await assert.rejects(stat(join(root, "new")), { code: "ENOENT" });
  await applyPatch("*** Begin Patch\n*** Update File: one\n*** Move to: nested/two\n@@\n first\n-second\n+third\n*** End Patch", root);
  assert.equal(await readFile(join(root, "nested/two"), "utf8"), "first\nthird\n");
  await assert.rejects(stat(join(root, "one")), { code: "ENOENT" });
  await writeFile(join(root, "ambiguous"), "same\nsame\n");
  await assert.rejects(applyPatch("*** Begin Patch\n*** Update File: ambiguous\n@@\n-same\n+changed\n*** End Patch", root), /ambiguous/);
  await symlink(tmpdir(), join(root, "outside"));
  await assert.rejects(applyPatch("*** Begin Patch\n*** Add File: outside/nope\n+no\n*** End Patch", root), /escapes/);
});

test("REPL keeps top-level await bindings across clients and recovers after errors and hangs", async t => {
  const root = await temporary(t), id = randomUUID();
  const calls = [];
  const options = { id, workspace: root, callTool: async (name, args) => { calls.push({ name, args }); return { answer: args.value + 1 }; } };
  const repl = new CodeRepl(options);
  t.after(() => repl.run({}, "close"));
  assert.equal((await repl.run({ code: 'let value = await Promise.resolve(41); const fs = await import("node:fs/promises");' })).isError, false);
  const second = new CodeRepl(options);
  let result = JSON.parse((await second.run({ code: 'text(await tools.test({value})); await fs.writeFile("proof", String(value + 1));' })).content);
  assert.match(result.text, /answer: 42/);
  assert.equal(calls.length, 1);
  assert.equal(await readFile(join(root, "proof"), "utf8"), "42");
  result = JSON.parse((await repl.run({ code: "while (true) {}", timeout_ms: 50 })).content);
  assert(result.isError && result.reset);
  result = JSON.parse((await repl.run({ code: "text(typeof value)" })).content);
  assert.match(result.text, /undefined/);
  assert.equal((await repl.run({ code: "let preserved = 7" })).isError, false);
  assert((await repl.run({ code: 'throw new Error("expected")' })).isError);
  result = JSON.parse((await repl.run({ code: "text(preserved)" })).content);
  assert.match(result.text, /7/);
});

test("conversation checkpoint locking and recovery never replay uncertain calls", async t => {
  const root = await temporary(t);
  const store = new ConversationStore(root, "test");
  await store.acquire();
  t.after(() => store.close());
  await assert.rejects(new ConversationStore(root, "test").acquire(), /already in use/);
  const input = [call("exec_command", { command: "some side effect" }, "uncertain")];
  await store.save({ input, history: [] });
  assert.equal((await stat(store.path)).mode & 0o777, 0o600);
  assert.equal(recoverInterruptedCalls(input), 1);
  assert.match(input.at(-1).output, /may have run/);
  assert.equal(recoverInterruptedCalls(input), 0);
});

test("killed runners release kernel locks and terminate owned shell descendants", async t => {
  const root = await temporary(t);
  const module = new URL("../src/storage.mjs", import.meta.url).href;
  const commandModule = new URL("../src/commands.mjs", import.meta.url).href;
  const child = spawn(process.execPath, ["--input-type=module", "-e", `
    const { ConversationStore } = await import(${JSON.stringify(module)});
    const store = new ConversationStore(${JSON.stringify(root)}, "crash");
    await store.acquire();
    const { CommandSessions } = await import(${JSON.stringify(commandModule)});
    const commands = new CommandSessions({workspace: ${JSON.stringify(root)}});
    await commands.exec({command: "touch command-started; (trap '' TERM; sleep 2; touch leaked-after-crash) & wait", yield_time_ms: 50});
    process.stdout.write("ready\\n");
    setInterval(() => {}, 1000);
  `], { stdio: ["ignore", "pipe", "pipe"] });
  t.after(() => child.kill("SIGKILL"));
  const childClosed = once(child, "close");
  await Promise.race([once(child.stdout, "data"), childClosed.then(() => { throw new Error("lock owner did not start"); })]);
  for (let attempt = 0; ; attempt++) {
    try { await stat(join(root, "command-started")); break; }
    catch (error) {
      if (error.code !== "ENOENT" || attempt > 100) throw error;
      await new Promise(resolve => setTimeout(resolve, 20));
    }
  }
  const contenders = Array.from({ length: 8 }, () => new ConversationStore(root, "crash"));
  const busy = await Promise.allSettled(contenders.map(store => store.acquire()));
  assert(busy.every(result => result.status === "rejected" && result.reason.code === "CONVERSATION_BUSY"));
  child.kill("SIGKILL");
  await childClosed;
  let acquired = false;
  for (let attempt = 0; attempt < 50 && !acquired; attempt++) {
    try { await contenders[0].acquire(); acquired = true; }
    catch (error) {
      if (error.code !== "CONVERSATION_BUSY") throw error;
      await new Promise(resolve => setTimeout(resolve, 20));
    }
  }
  assert(acquired, "a killed runner must release the kernel lock");
  await contenders[0].close();
  const results = await Promise.allSettled(contenders.map(store => store.acquire()));
  assert.equal(results.filter(result => result.status === "fulfilled").length, 1);
  await Promise.all(contenders.map(store => store.close()));
  await new Promise(resolve => setTimeout(resolve, 2100));
  await assert.rejects(stat(join(root, "leaked-after-crash")), { code: "ENOENT" });
});

test("real provider requests edit a workspace through REPL and resume with tool state intact", async t => {
  let fixtureState;
  fixtureState = await fixture(t, async (body, response, path, count) => {
    assert.equal(path, "/v1/responses");
    if (count === 1) fixtureState.send(response, [call("js_repl", { code: 'let count = 41; text(await tools.apply_patch({patch: "*** Begin Patch\\n*** Add File: answer.txt\\n+42\\n*** End Patch"}));' }, "edit")]);
    else if (count === 2) fixtureState.send(response, [message("Created answer.txt")]);
    else {
      assert(body.input.some(item => item.type === "function_call_output" && item.call_id === "edit"));
      assert(body.input.some(item => item.type === "message" && item.phase === "final_answer"));
      fixtureState.send(response, [message("History retained")]);
    }
  });
  const { options, workspace, events, requests } = fixtureState;
  await writeFile(join(workspace, "AGENTS.md"), "Use this repository's verification command.");
  await runAgent({ prompt: "Create the answer" }, options);
  const id = events.find(event => event.type === "session").conversation_id;
  t.after(() => new CodeRepl({ id, workspace, callTool: () => {} }).run({}, "close"));
  assert.equal(await readFile(join(workspace, "answer.txt"), "utf8"), "42\n");
  await runAgent({ conversation_id: id, prompt: "What changed?" }, options);
  assert.equal(requests.length, 3);
  assert.equal(requests[0].headers.authorization, "Bearer fixture-secret");
  assert.match(requests[0].body.instructions, /verification command/);
  const state = JSON.parse(await readFile(join(options.stateDirectory, `${id}.json`), "utf8"));
  assert.equal(state.history.length, 2);
  assert.equal(state.status, "idle");
});

test("failed tools are returned to the model and commentary does not end a turn", async t => {
  const { options } = await fixture(t, async (body, response, path, count) => {
    const output = count === 1 ? [call("exec_command", "{invalid", "bad")]
      : count === 2 ? [message("I will check that", "commentary")]
      : [message("Recovered")];
    if (count === 2) assert.match(body.input.at(-1).output[0].text, /Tool failed/);
    response.writeHead(200, { "content-type": "application/json" }); response.end(JSON.stringify(completed(output)));
  });
  const history = await runAgent({ prompt: "Recover" }, options);
  assert.equal(history.at(-1).assistant, "Recovered");
});

test("manual remote compaction preserves opaque state across process turns", async t => {
  let f;
  f = await fixture(t, async (body, response, path, count) => {
    if (path.endsWith("/compact")) {
      assert(body.input.some(item => item.role === "user"));
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ output: [body.input[0], { type: "compaction", encrypted_content: "opaque-checkpoint" }], usage: { input_tokens: 100, output_tokens: 10 } }));
    } else {
      if (count === 3) assert(body.input.some(item => item.encrypted_content === "opaque-checkpoint"));
      f.send(response, [message("Done")]);
    }
  });
  await runAgent({ prompt: "Remember this task" }, f.options);
  const id = f.events[0].conversation_id;
  await runAgent({ conversation_id: id, prompt: "/compact" }, f.options);
  await runAgent({ conversation_id: id, prompt: "Continue" }, f.options);
  assert.equal(f.events.filter(event => event.type === "compaction").length, 1);
});

test("automatic compaction runs before the model's context limit", async t => {
  let f;
  f = await fixture(t, async (body, response, path) => {
    if (path.endsWith("/compact")) {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ output: [{ type: "compaction", encrypted_content: "reduced" }, body.input.at(-1)] }));
    } else f.send(response, [message("Done")]);
  });
  f.options.config.compact_threshold = 10000;
  f.options.config.context_window = 16000;
  await runAgent({ prompt: "Continue", history: [{ user: "Original task", assistant: "detail ".repeat(6000) }] }, f.options);
  assert(f.requests[0].path.endsWith("/compact"));
  assert(f.requests[1].body.input.some(item => item.type === "compaction"));
});

test("summary fallback preserves the current request and complete recent tool pairs", async t => {
  let f;
  f = await fixture(t, async (body, response, path) => {
    if (path.endsWith("/compact")) { response.writeHead(404); response.end("unsupported"); }
    else if (body.instructions.includes("Produce") || body.input.at(-1).content === "Produce the continuation checkpoint now.") f.send(response, [message("Original task: continue the project. Completed earlier inspections. Next: honor the latest correction.")]);
    else {
      assert(body.input.some(item => JSON.stringify(item).includes("latest correction")));
      assert(body.input.some(item => JSON.stringify(item).includes("Continuation checkpoint")));
      f.send(response, [message("Resumed from summary")]);
    }
  });
  f.options.config.compact_threshold = 10000;
  f.options.config.context_window = 64000;
  const history = Array.from({ length: 14 }, (_, index) => ({ user: `Task ${index}`, assistant: "earlier detail ".repeat(450) }));
  await runAgent({ history, prompt: "latest correction" }, f.options);
  assert(f.events.some(event => event.type === "compaction" && event.method === "summary"));
});

test("failed compaction leaves the original checkpoint intact", async t => {
  let f;
  f = await fixture(t, async (body, response, path) => {
    if (path.endsWith("/compact")) { response.writeHead(200, { "content-type": "application/json" }); response.end('{"output":[]}'); }
    else f.send(response, [message("Initial answer")]);
  });
  await runAgent({ prompt: "Original task" }, f.options);
  const id = f.events[0].conversation_id, path = join(f.options.stateDirectory, `${id}.json`);
  const before = JSON.parse(await readFile(path, "utf8")).input;
  await assert.rejects(runAgent({ conversation_id: id, prompt: "/compact" }, f.options), /no valid compaction item/);
  assert.deepEqual(JSON.parse(await readFile(path, "utf8")).input, before);
});

test("cancellation checkpoints uncertain actions and resume inspects instead of replaying", async t => {
  let f;
  f = await fixture(t, async (body, response, path, count) => {
    if (count === 1) f.send(response, [call("exec_command", { command: "printf once >> effects; sleep 10", yield_time_ms: 10000 }, "uncertain")]);
    else {
      const recovered = body.input.find(item => item.type === "function_call_output" && item.call_id === "uncertain");
      assert.match(recovered.output, /may have run/);
      f.send(response, [message("Inspect the existing effect")]);
    }
  });
  const controller = new AbortController();
  const running = assert.rejects(runAgent({ prompt: "Perform one action" }, { ...f.options, signal: controller.signal }), /cancelled/);
  for (let attempt = 0; attempt < 100; attempt++) {
    if (await stat(join(f.workspace, "effects")).then(() => true, () => false)) break;
    await new Promise(resolve => setTimeout(resolve, 10));
  }
  controller.abort(new Error("cancelled"));
  await running;
  const id = f.events[0].conversation_id;
  await runAgent({ conversation_id: id, prompt: "Continue safely" }, f.options);
  assert.equal(await readFile(join(f.workspace, "effects"), "utf8"), "once");
});

test("shared OAuth credentials refresh once across simultaneous agents", async t => {
  const root = await temporary(t);
  const auth = join(root, "agent-auth.json");
  const jwt = seconds => `e30.${Buffer.from(JSON.stringify({ exp: Math.floor(Date.now() / 1000) + seconds })).toString("base64url")}.test`;
  await writeFile(auth, JSON.stringify({ access_token: jwt(-10), refresh_token: "old-refresh", account_id: "fixture" }));
  let refreshes = 0;
  const fetchImpl = async () => {
    refreshes++;
    await new Promise(resolve => setTimeout(resolve, 50));
    return new Response(JSON.stringify({ access_token: jwt(3600), refresh_token: "new-refresh" }), { headers: { "content-type": "application/json" } });
  };
  const env = { BROOD_AGENT_AUTH: auth, HOME: root };
  const [one, two] = await Promise.all([resolveModelBackend(env, fetchImpl), resolveModelBackend(env, fetchImpl)]);
  assert.equal(one.headers.authorization, two.headers.authorization);
  assert.equal(refreshes, 1);
  assert.equal((await stat(auth)).mode & 0o777, 0o600);
});

test("a rejected OAuth token is refreshed once without spending a transient retry", async () => {
  let attempts = 0, refreshes = 0;
  const backend = { url: "http://fixture", headers: {}, async refresh() { refreshes++; } };
  const response = await requestModel(async () => ++attempts === 1 ? new Response("expired", { status: 401 }) : sse([message("ok")]), backend, {}, { settings: { max_retries: 0 } });
  assert.equal(refreshes, 1);
  assert.equal(attempts, 2);
  assert.equal(response.output[0].content[0].text, "ok");
});

test("computer observations nested in the code REPL reach the model as images", async t => {
  const observation = { type: "input_image", image_url: "data:image/png;base64,iVBORw0KGgo=" };
  let f;
  f = await fixture(t, async (body, response, path, count) => {
    if (count === 1) f.send(response, [call("js_repl", { code: 'text(await tools.computer_observe({task:"test"}));' }, "observe-cell")]);
    else {
      const result = body.input.find(item => item.call_id === "observe-cell" && item.type === "function_call_output");
      assert.deepEqual(result.output.find(item => item.type === "input_image"), observation);
      f.send(response, [message("Observed the image")]);
    }
  });
  let called = false;
  f.options.computerUse = {
    tools: () => [{ type: "function", name: "computer_observe", parameters: { type: "object", properties: {} } }],
    call: async (name, args, id, signal) => {
      assert.equal(name, "computer_observe");
      assert(signal instanceof AbortSignal);
      assert.equal(signal.aborted, false);
      called = true;
      return { text: '{"observed":true}', output: [{ type: "input_text", text: "observed" }, observation] };
    },
  };
  await runAgent({ prompt: "Observe", conversation_id: "image-cell" }, f.options);
  t.after(() => new CodeRepl({ id: "image-cell", workspace: f.workspace, callTool() {} }).run({}, "close"));
  assert(called);
});
