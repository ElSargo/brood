import { readFile, realpath } from "node:fs/promises";
import { join, dirname } from "node:path";
import { randomUUID, createHash } from "node:crypto";
import { NativeComputerUseClient } from "./computer-use.mjs";
import { providerReadiness, resolveModelBackend } from "./auth.mjs";
import { BUILTIN_PROVIDERS, loadConfig, resolveSettings } from "./config.mjs";
import { ConversationStore, stateDirectory, recoverInterruptedCalls } from "./storage.mjs";
import { requestModel } from "./transport.mjs";
import { CommandSessions } from "./commands.mjs";
import { CodeRepl } from "./repl.mjs";
import { applyPatch } from "./patch.mjs";
import { compactConversation, estimateTokens } from "./compaction.mjs";
export { resolveModelBackend } from "./auth.mjs";
export { parseSsePayloads } from "./transport.mjs";
export { runExecCommand } from "./commands.mjs";

const MAX_TOOL_OUTPUT_BYTES = 1024 * 1024;
const COMPUTER_USE_INSTRUCTIONS = "You are operating inside a Brood session. Computer use is provided as native computer_* tools scoped to this session. For requests involving a graphical application, call computer_windows first and reuse a suitable existing window. Call computer_launch only when no suitable window exists. Never launch a graphical application through exec_command: a shell process is not a Brood-managed application and cannot reliably connect to the session display. Start one task, then prefer computer_exec_js for multi-step interaction, repeated actions, waits, and screenshot checks; its JavaScript global state persists across calls. Use computer_action for short corrective actions. Batch actions that do not require an intervening observation, but never guess after the UI changes. Do not claim that a new application opened unless computer_launch returned visible=true. Treat every returned screenshot as authoritative evidence of the action's actual effect. To replace text in an application entry, click the intended entry, use Ctrl+A, and type the replacement; do not guess application-specific shortcuts. If an unexpected window or dialog appears, recover and observe again. Do not stop automation or report success until a screenshot visibly shows the requested outcome.";

const BASE_INSTRUCTIONS = `You are Brood's independent session coding agent. Work through the user's requested outcome until implementation and appropriate verification are complete. Preserve unrelated work. Inspect relevant files and repository instructions before changing them, use rg or rg --files for search, and use apply_patch for edits. Respect AGENTS.md instructions for each directory you work in. Use plan_update for work with dependent steps and keep it current. Use exec_command for files, builds, tests, and repository inspection. Commands may return a session_id: use write_stdin to wait, send input, or terminate. Keep active commands accounted for before finishing. The persistent Node.js js_repl supports top-level await, imports, let/const bindings, text(value), and await tools.TOOL_NAME(args). Its variables survive compaction and subsequent turns while the REPL process is alive; timeout, cancellation, reset, or 30 minutes idle clears them. Never assume a binding survived a reset. Await every tool promise, including Promise.all for independent reads; keep dependent operations and edits sequential. js_repl is ordinary session code execution, not a security sandbox. Checkpoint recovery explicitly marks calls with uncertain outcomes: inspect actual effects before repeating them. Preserve the user's objective across compaction. Report verified results and remaining limitations honestly. Keep user-facing prose concise.\n\n${COMPUTER_USE_INSTRUCTIONS}`;

const EXEC_COMMAND_TOOL = {
  type: "function",
  name: "exec_command",
  description: "Run a shell command in the session workspace. Returns bounded output and a session_id when still running. Continue with write_stdin. Output artifacts preserve longer results. Prefer rg for search. The shell inherits the session environment without login startup files by default.",
  parameters: {
    type: "object",
    properties: {
      command: { type: "string", description: "Shell command to run." },
      workdir: { type: "string", description: "Workspace-relative or absolute working directory. Must remain inside the session workspace." },
      timeout_ms: { type: "integer", minimum: 1, maximum: 3600000 },
      yield_time_ms: { type: "integer", minimum: 0, maximum: 30000 },
      login: { type: "boolean" },
      max_output_bytes: { type: "integer", minimum: 1024, maximum: MAX_TOOL_OUTPUT_BYTES },
    },
    required: ["command"],
    additionalProperties: false,
  },
  strict: false,
};

const PLAN_UPDATE_TOOL = {
  type: "function",
  name: "plan_update",
  description: "Publish or update the concise execution plan shown to the user. Use only for multi-step work and keep exactly one step in_progress until all work is completed.",
  parameters: {
    type: "object",
    required: ["steps"],
    properties: {
      explanation: { type: "string" },
      steps: {
        type: "array",
        minItems: 1,
        maxItems: 12,
        items: {
          type: "object",
          required: ["step", "status"],
          properties: {
            step: { type: "string", minLength: 1, maxLength: 200 },
            status: { enum: ["pending", "in_progress", "completed"] },
          },
          additionalProperties: false,
        },
      },
    },
    additionalProperties: false,
  },
  strict: false,
};

function tool(name, description, properties, required = []) {
  return { type: "function", name, description, parameters: { type: "object", properties, required, additionalProperties: false }, strict: false };
}
const EXTRA_TOOLS = [
  tool("write_stdin", "Poll a running command session, send stdin, close stdin, or terminate its process group. Output contains only bytes since the previous read.", {
    session_id: { type: "string" }, chars: { type: "string" }, close_stdin: { type: "boolean" }, terminate: { type: "boolean" }, yield_time_ms: { type: "integer", minimum: 0, maximum: 30000 },
  }, ["session_id"]),
  tool("apply_patch", "Apply a patch to workspace files. Format: *** Begin Patch, *** Add/Update/Delete File: relative/path, hunks with space/+/- lines, *** End Patch. Update hunks may use @@ or @@ exact anchor text and *** End of File. Supports *** Move to: relative/path. Exact context must be unambiguous. All files are checked before writing.", { patch: { type: "string" } }, ["patch"]),
  tool("js_repl", "Execute JavaScript in a persistent Node.js REPL scoped to this conversation. Top-level await, imports, and let/const bindings persist across calls, turns, and compaction until reset or idle expiry. text(value) prints bounded output; await tools.TOOL_NAME(args) calls the agent tools and returns their structured result. Await all tool promises. Use js_repl_reset when a fresh kernel is needed. Images from nested computer tools are forwarded with the cell result. Timeout or cancellation resets the kernel.", { code: { type: "string" }, timeout_ms: { type: "integer", minimum: 1, maximum: 120000 } }, ["code"]),
  tool("js_repl_reset", "Reset this conversation's JavaScript REPL and clear all variables.", {}),
];

function bytePrefix(text = "", maximum = 4000) {
  const bytes = Buffer.from(text);
  return bytes.length <= maximum ? text : `${bytes.subarray(0, maximum).toString("utf8")}\n[output truncated]`;
}
function textFromResponse(output) {
  return output.filter(item => item.type === "message").flatMap(item => item.content ?? []).filter(item => item.type === "output_text").map(item => item.text ?? "").join("\n");
}
function addUsage(total, usage = {}) {
  total.requests++;
  total.input_tokens += Number(usage.input_tokens) || 0;
  total.cached_input_tokens += Number(usage.input_tokens_details?.cached_tokens) || 0;
  total.output_tokens += Number(usage.output_tokens) || 0;
  total.reasoning_tokens += Number(usage.output_tokens_details?.reasoning_tokens) || 0;
}
function priorMessages(history) {
  return (history ?? []).flatMap(turn => [
    { role: "user", content: [{ type: "input_text", text: turn.user }] },
    { role: "assistant", content: [{ type: "output_text", text: turn.assistant }] },
  ]);
}

async function repositoryInstructions(workspace) {
  const directories = [];
  for (let path = workspace; ; path = dirname(path)) {
    directories.unshift(path);
    if (dirname(path) === path) break;
  }
  const instructions = [];
  let size = 0;
  for (const directory of directories) {
    const path = join(directory, "AGENTS.md");
    const content = await readFile(path, "utf8").catch(error => { if (error.code === "ENOENT") return ""; throw error; });
    if (!content) continue;
    size += Buffer.byteLength(content);
    if (size > 128 * 1024) throw new Error("Repository instructions exceed 128 KiB; reduce AGENTS.md size before running");
    instructions.push(`Repository instructions from ${path}:\n${content}`);
  }
  return instructions.join("\n\n");
}

function parseCommand(prompt) {
  const match = /^\/(help|status|compact|provider|model|reasoning|profile|new|resume)(?:\s+(.*))?$/s.exec(prompt?.trim() ?? "");
  return match ? { name: match[1], value: match[2]?.trim() ?? "" } : undefined;
}

async function providerSummary(config, env) {
  const readiness = await providerReadiness(env);
  const entries = BUILTIN_PROVIDERS.map(name => readiness[name] ? `${name} (no ${readiness[name]})` : `${name} (ready)`);
  for (const [name, provider] of Object.entries(config.providers ?? {})) {
    const missing = [];
    if (provider.env_key && !env[provider.env_key]) missing.push(provider.env_key);
    for (const variable of Object.values(provider.env_http_headers ?? {})) if (!env[variable]) missing.push(variable);
    entries.push(missing.length ? `${name} (no ${missing.join(", ")})` : `${name} (ready)`);
  }
  return entries.join(", ");
}

export async function runAgent(request, options = {}) {
  if (!request || typeof request !== "object" || typeof request.prompt !== "string") throw new Error("Agent request requires a prompt string");
  const env = options.env ?? process.env;
  const fetchImpl = options.fetchImpl ?? fetch;
  const signal = options.signal;
  signal?.throwIfAborted();
  const emit = options.emit ?? ((type, fields = {}) => process.stdout.write(`${JSON.stringify({ type, ...fields })}\n`));
  const config = options.config ?? await loadConfig(env);
  const workspace = await realpath(options.workspace ?? process.cwd());
  const command = parseCommand(request.prompt);
  const id = command?.name === "new" ? randomUUID() : command?.name === "resume" ? command.value : request.conversation_id || randomUUID();
  const store = options.persist === false ? undefined : new ConversationStore(options.stateDirectory ?? stateDirectory(env), id);
  let computerUse, commands, repl, state;
  let finalText = "";
  const usage = { requests: 0, input_tokens: 0, cached_input_tokens: 0, output_tokens: 0, reasoning_tokens: 0 };
  const save = () => store?.save(state);
  try {
    await store?.acquire();
    state = await store?.read();
    if (command?.name === "resume" && !state) throw new Error(`Conversation ${id} does not exist`);
    if (state && state.workspace !== workspace) throw new Error("Conversation belongs to a different workspace");
    state ??= { version: 1, id, workspace, input: command?.name === "new" ? [] : priorMessages(request.history), history: command?.name === "new" ? [] : request.history ?? [], preferences: {}, status: "idle", compactions: 0 };
    const recovered = recoverInterruptedCalls(state.input);
    if (recovered || state.status === "running") {
      state.input.push({ role: "user", content: "The prior turn was interrupted. Continue from its checkpoint, preserving completed work. Check actual state before retrying actions with uncertain outcomes. Shell sessions from the previous runner have ended; REPL bindings may or may not remain." });
      emit("status", { text: "Recovered an interrupted turn; completed tool results retained" });
    }
    const overrides = { ...state.preferences };
    for (const key of Object.keys(request)) if (request[key] !== "" && request[key] !== "default") overrides[key] = request[key];
    if (command && ["provider", "model", "reasoning", "profile"].includes(command.name)) {
      if (!command.value || /\s/.test(command.value)) throw new Error(`/${command.name} requires one name`);
      if (command.name === "profile") {
        for (const key of ["model", "provider", "reasoning"]) delete overrides[key];
      }
      overrides[command.name] = command.value;
    }
    const settings = resolveSettings(config, overrides, env);
    state.preferences = { provider: settings.provider, model: settings.model, reasoning: settings.reasoning, ...(overrides.profile ? { profile: overrides.profile } : {}) };
    state.status = "idle";
    await save();
    emit("session", { conversation_id: id, text: `Conversation ${id}` });
    emit("configuration", { provider: settings.provider, model: settings.model, reasoning: settings.reasoning, profile: overrides.profile ?? "", text: `Provider ${settings.provider} · model ${settings.model} · reasoning ${settings.reasoning}` });
    const result = () => {
      emit("result", { conversation_id: id, history: state.history.slice(-24), usage, compactions: state.compactions });
      return state.history.slice(-24);
    };
    if (command && command.name !== "compact") {
      if (["new", "resume"].includes(command.name)) {
        emit("transcript_reset", { text: "Conversation opened" });
        for (const turn of state.history.slice(-24)) {
          emit("history_user", { text: turn.user });
          emit("assistant", { text: turn.assistant });
        }
      }
      const text = command.name === "help"
        ? "Commands: /status, /compact, /provider NAME, /model NAME, /reasoning LEVEL, /profile NAME, /new, /resume ID. Configure providers in the Brood agent.json file; credentials come from environment variables."
        : command.name === "status"
          ? `Conversation ${id}\nProvider: ${settings.provider}\nModel: ${settings.model}\nReasoning: ${settings.reasoning}\nContext estimate: ${estimateTokens(state.input)} / ${settings.context_window} tokens\nCompactions: ${state.compactions}\nProviders: ${await providerSummary(config, env)}\nProfiles: ${Object.keys(config.profiles ?? {}).join(", ") || "none"}`
          : command.name === "new" ? "New conversation started" : command.name === "resume" ? "Conversation resumed" : "Agent configuration updated";
      emit("assistant", { text });
      return result();
    }
    const backend = options.backend ?? await resolveModelBackend(env, fetchImpl, settings, signal, { session: id });
    const backendIdentity = createHash("sha256").update(`${backend.kind ?? settings.provider}\0${backend.url}`).digest("hex");
    if (state.backend && state.backend !== backendIdentity && state.input.length) throw new Error("This conversation contains state from a different provider endpoint. Start /new before switching providers to keep opaque reasoning and compaction state valid.");
    state.backend = backendIdentity;
    computerUse = options.computerUse ?? (env.BROOD_NATIVE_TOOL_SOCKET
      ? new NativeComputerUseClient({ socketPath: env.BROOD_NATIVE_TOOL_SOCKET, session: request.session_id, principal: request.agent, signal })
      : { tools: () => [], close: async () => {} });
    const definitions = [EXEC_COMMAND_TOOL, PLAN_UPDATE_TOOL, ...EXTRA_TOOLS, ...await computerUse.tools()];
    const instructions = [BASE_INSTRUCTIONS, `Workspace: ${workspace}`, await repositoryInstructions(workspace)].filter(Boolean).join("\n\n");
    const body = {
      model: settings.model, store: false, stream: true, instructions, tools: definitions,
      tool_choice: "auto", parallel_tool_calls: false,
      ...(backend.supportsReasoning === false ? {} : { reasoning: { effort: settings.reasoning, summary: "auto" }, include: ["reasoning.encrypted_content"] }),
      ...(backend.supportsVerbosity === false ? {} : { text: { verbosity: "low" } }),
      prompt_cache_key: id,
    };
    const compact = async () => {
      const input = await compactConversation(state.input, body, { backend, fetchImpl, settings, signal, emit, retryBaseMs: options.modelRetryBaseMs, onUsage: value => addUsage(usage, value) });
      if (input === state.input) return false;
      state.input = input;
      state.compactions++;
      state.context = undefined;
      await save();
      return true;
    };
    if (command?.name === "compact") { await compact(); return result(); }
    if (!request.prompt.trim() && !request.images?.length) throw new Error("Prompt is empty");
    const content = [{ type: "input_text", text: request.prompt }];
    let imageBytes = 0;
    if ((request.images?.length ?? 0) > 4) throw new Error("At most four images may be attached");
    for (const image of request.images ?? []) {
      if (!["image/png", "image/jpeg", "image/gif", "image/webp"].includes(image.mime_type)) throw new Error("Unsupported image MIME type");
      const bytes = await readFile(image.path);
      imageBytes += bytes.length;
      if (bytes.length > 8 * 1024 * 1024 || imageBytes > 12 * 1024 * 1024) throw new Error("Image attachment size limit exceeded");
      content.push({ type: "input_image", image_url: `data:${image.mime_type};base64,${bytes.toString("base64")}` });
    }
    state.input.push({ role: "user", content });
    state.status = "running";
    await save();
    commands = new CommandSessions({ workspace, signal, outputBytes: settings.tool_output_bytes, artifactDirectory: store ? join(dirname(store.path), "artifacts", id) : undefined });
    let replImages = [];
    const executeTool = async (name, args, toolSignal = signal) => {
      toolSignal?.throwIfAborted();
      if (!args || typeof args !== "object" || Array.isArray(args)) throw new Error("Tool arguments must be an object");
      if (name === "exec_command") return commands.exec(args, toolSignal);
      if (name === "write_stdin") return commands.write(args, toolSignal);
      if (name === "apply_patch") return { content: await applyPatch(args.patch, workspace, toolSignal), isError: false };
      if (name === "js_repl") {
        replImages = [];
        const value = await repl.run(args);
        if (replImages.length) value.output = [{ type: "input_text", text: value.content }, ...replImages];
        return value;
      }
      if (name === "js_repl_reset") return repl.run({}, "reset");
      if (name === "plan_update") {
        if (!Array.isArray(args.steps) || args.steps.length < 1 || args.steps.length > 12 || args.steps.some(step => typeof step.step !== "string" || !step.step.trim() || step.step.length > 200 || !["pending", "in_progress", "completed"].includes(step.status))) throw new Error("Plan must contain 1–12 valid steps");
        if (args.steps.filter(step => step.status === "in_progress").length > 1) throw new Error("Plan may contain at most one in_progress step");
        state.plan = args.steps;
        emit("plan", { text: args.explanation || "Plan updated", steps: args.steps });
        await save();
        return { content: JSON.stringify(args.steps), isError: false };
      }
      if (definitions.some(definition => definition.name === name) && name.startsWith("computer_")) return computerUse.call(name, args, randomUUID(), toolSignal);
      throw new Error(`Unknown tool ${name}`);
    };
    const runTool = async (name, args, toolSignal) => {
      emit("tool_start", { tool: name, text: name === "exec_command" ? bytePrefix(args.command ?? args.cmd, 180) : "", arguments: args });
      try {
        const value = await executeTool(name, args, toolSignal);
        emit("tool_end", { tool: name, text: bytePrefix(value.summary ?? value.text ?? value.content), is_error: !!value.isError });
        return value;
      } catch (error) {
        emit("tool_end", { tool: name, text: error.message, is_error: true });
        if (signal?.aborted) throw error;
        return { content: `Tool failed: ${error.message}`, isError: true };
      }
    };
    repl = new CodeRepl({ id, workspace, signal, outputBytes: settings.tool_output_bytes, callTool: async (name, args, cellSignal) => {
      const value = await runTool(name, args, cellSignal);
      for (const part of value.output ?? []) {
        if (part.type !== "input_image") continue;
        replImages.push(part);
        while (replImages.length > 4 || (replImages.length && Buffer.byteLength(JSON.stringify(replImages)) > 16 * 1024 * 1024)) replImages.shift();
      }
      let output = value.content ?? value.text;
      try { output = JSON.parse(output); } catch {}
      return { output, isError: !!value.isError };
    } });
    emit("status", { text: `Starting Brood agent · model=${settings.model} · reasoning=${settings.reasoning}` });
    let compactedAt = -1;
    let uncompactedEstimate = 0;
    for (let step = 1; step <= settings.max_steps; step++) {
      signal?.throwIfAborted();
      const estimated = estimateTokens(state.input);
      const measured = state.context ? state.context.tokens + Math.max(0, estimated - state.context.estimate) : 0;
      const contextTokens = Math.max(estimated + estimateTokens(instructions) + estimateTokens(definitions), measured);
      emit("context", { text: `Context ${contextTokens} / ${settings.context_window} tokens`, estimated_tokens: contextTokens, context_window: settings.context_window });
      if (contextTokens >= settings.compact_threshold && (backend.compaction ?? settings.compaction) !== "off" && compactedAt !== state.input.length && estimated >= uncompactedEstimate * 1.25) {
        uncompactedEstimate = await compact() ? 0 : estimated;
        compactedAt = state.input.length;
      }
      let completed;
      let lastActivity = 0;
      const onEvent = event => {
        if (event.type.endsWith(".delta") && Date.now() - lastActivity > 10000) {
          emit("status", { text: "Model is responding" }); lastActivity = Date.now();
        }
      };
      try {
        completed = await requestModel(fetchImpl, backend, { ...body, input: state.input }, { signal, settings, retryBaseMs: options.modelRetryBaseMs, emit, onEvent });
      } catch (error) {
        if (["context_length_exceeded", "context_window_exceeded"].includes(error.code) && compactedAt !== state.input.length) {
          if (!await compact()) throw new Error("Context limit reached and compaction could not reduce it; original checkpoint retained");
          compactedAt = state.input.length;
          completed = await requestModel(fetchImpl, backend, { ...body, input: state.input }, { signal, settings, retryBaseMs: options.modelRetryBaseMs, emit, onEvent });
        } else throw error;
      }
      addUsage(usage, completed.usage);
      const output = completed.output;
      if (!Array.isArray(output) || output.length === 0) throw new Error("Model returned an empty response; conversation checkpoint retained");
      const calls = output.filter(item => item.type === "function_call");
      const callIds = new Set();
      const oldIds = new Set(state.input.filter(item => item.type === "function_call").map(item => item.call_id));
      for (const call of calls) {
        if (!call.call_id || !call.name || callIds.has(call.call_id) || oldIds.has(call.call_id)) throw new Error("Model returned missing or duplicate tool call identifiers; no tools executed");
        callIds.add(call.call_id);
      }
      state.input.push(...output);
      state.context = { tokens: (completed.usage?.input_tokens ?? 0) + (completed.usage?.output_tokens ?? 0), estimate: estimateTokens(state.input) };
      await save(); // Journal calls before executing any side effects.
      const reasoning = output.filter(item => item.type === "reasoning").flatMap(item => item.summary ?? []).map(item => item.text ?? "").join("\n");
      if (reasoning) emit("reasoning", { text: reasoning });
      const text = textFromResponse(output);
      if (text) emit("assistant", { text });
      for (const call of calls) {
        signal?.throwIfAborted();
        let value;
        try { value = await runTool(call.name, JSON.parse(call.arguments || "{}")); }
        catch (error) {
          if (signal?.aborted) throw error;
          value = { content: `Tool failed: ${error.message}`, isError: true };
        }
        state.input.push({ type: "function_call_output", call_id: call.call_id, output: value.output ?? [{ type: "input_text", text: value.content ?? value.text ?? "Tool completed" }] });
        await save();
      }
      if (calls.length) continue;
      const messages = output.filter(item => item.type === "message");
      if (messages.length && messages.every(item => item.phase === "commentary")) continue;
      if (commands.sessions.size) {
        state.input.push({ role: "user", content: `Commands still need their results collected: ${[...commands.sessions.keys()].join(", ")}. Use write_stdin to collect output and verify their outcomes before completing this task.` });
        await save();
        continue;
      }
      if (!text && !messages.some(item => item.content?.some(part => part.type === "refusal"))) throw new Error("Model stopped without an answer or tool call");
      finalText = text;
      state.history.push({ user: request.prompt, assistant: finalText });
      state.status = "idle";
      state.usage = usage;
      await save();
      emit("status", { text: "Turn complete" });
      return result();
    }
    throw new Error(`Agent reached its configured limit of ${settings.max_steps} model steps; progress saved for the next turn`);
  } catch (error) {
    if (state && store?.lock) {
      state.status = signal?.aborted ? "cancelled" : "failed";
      state.usage = usage;
      await save();
    }
    emit("error", { text: error.message, conversation_id: id });
    throw error;
  } finally {
    try { await commands?.close(); }
    finally {
      try { if (computerUse && !options.computerUse) await computerUse.close(); }
      finally { await store?.close(); }
    }
  }
}

export function createAbortController() {
  const controller = new AbortController();
  const abort = () => controller.abort(new Error("Turn cancelled"));
  for (const event of ["SIGINT", "SIGTERM"]) process.once(event, abort);
  controller.dispose = () => { for (const event of ["SIGINT", "SIGTERM"]) process.removeListener(event, abort); };
  return controller;
}

export function requestId() { return randomUUID(); }
