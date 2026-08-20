import { connect } from "node:net";

const SOCKET_ENV = "BROOD_NATIVE_TOOL_SOCKET";
const PROTOCOL_VERSION = 1;
const DEFAULT_LEASE_DURATION_MS = 300_000;
const DEFAULT_LAUNCH_TIMEOUT_MS = 10_000;
const MAX_LAUNCH_TIMEOUT_MS = 30_000;
const MAX_LAUNCH_ARGUMENTS = 32;
const MAX_LAUNCH_ARGUMENT_BYTES = 16 * 1024;
const MAX_RESPONSE_BYTES = 64 * 1024 * 1024;

const DEFAULT_POLICY = { human_input: "pause", require_approval: [] };
const DEFAULT_LIMITS = {
  max_actions_per_batch: 64,
  max_script_wall_ms: 30_000,
  max_script_cpu_ms: 5_000,
  max_script_memory_bytes: 64 * 1024 * 1024,
  max_output_bytes: 1024 * 1024,
  max_display_images: 16,
  max_queued_host_calls: 64,
};

function coordinateSchema() {
  return { type: "number", minimum: 0 };
}

function actionSchema() {
  const coordinate = coordinateSchema();
  const button = { enum: ["left", "middle", "right"] };
  const point = {
    type: "object",
    required: ["x", "y"],
    properties: { x: coordinate, y: coordinate },
  };
  return {
    oneOf: [
      { type: "object", required: ["type", "x", "y", "button"], properties: { type: { const: "click" }, x: coordinate, y: coordinate, button } },
      { type: "object", required: ["type", "x", "y", "button"], properties: { type: { const: "double_click" }, x: coordinate, y: coordinate, button } },
      { type: "object", required: ["type", "x", "y"], properties: { type: { const: "move" }, x: coordinate, y: coordinate } },
      { type: "object", required: ["type", "path", "button"], properties: { type: { const: "drag" }, path: { type: "array", minItems: 2, items: point }, button } },
      { type: "object", required: ["type", "x", "y", "delta_x", "delta_y"], properties: { type: { const: "scroll" }, x: coordinate, y: coordinate, delta_x: { type: "number" }, delta_y: { type: "number" } } },
      { type: "object", required: ["type", "text"], properties: { type: { const: "type" }, text: { type: "string" } } },
      { type: "object", required: ["type", "keys"], properties: { type: { const: "keypress" }, keys: { type: "array", minItems: 1, items: { type: "string" } } } },
      { type: "object", required: ["type", "duration_ms"], properties: { type: { const: "wait" }, duration_ms: { type: "integer", minimum: 0, maximum: 30_000 } } },
      { type: "object", required: ["type"], properties: { type: { const: "screenshot" } } },
    ],
  };
}

function functionTool(name, description, parameters) {
  return { type: "function", name, description, parameters, strict: false };
}

export function computerUseTools(session) {
  const tools = [
    functionTool("computer_windows", `List existing GUI windows in Brood session ${session}. Check these windows first and reuse a suitable one before launching another application`, {
      type: "object",
      properties: {},
    }),
    functionTool("computer_launch", `Launch a GUI application inside Brood session ${session} and wait for its managed window. Call computer_windows first and launch only when no suitable window already exists`, {
      type: "object",
      required: ["command"],
      properties: {
        command: { type: "string", minLength: 1, maxLength: 256 },
        args: { type: "array", maxItems: MAX_LAUNCH_ARGUMENTS, items: { type: "string" } },
        window_timeout_ms: { type: "integer", minimum: 1, maximum: MAX_LAUNCH_TIMEOUT_MS },
      },
    }),
    functionTool("computer_start", `Start a computer-use task bound to this agent's Brood session ${session}`, {
      type: "object",
      properties: {
        task: { type: "string" },
        windows: { type: "array", items: { type: "integer" } },
        active_window: { type: ["integer", "null"] },
        lease_duration_ms: { type: "integer", minimum: 1 },
        policy: { type: "object" },
        limits: { type: "object" },
      },
    }),
    functionTool("computer_action", "Execute ordered direct actions and return a causally newer screenshot. Inspect that screenshot as authoritative evidence and continue correcting the UI until the requested outcome is visibly present", {
      type: "object",
      required: ["protocol_version", "task", "lease_generation", "expected_sequence", "actions"],
      properties: {
        protocol_version: { const: PROTOCOL_VERSION },
        task: { type: "string" },
        lease_generation: { type: "integer" },
        expected_sequence: { type: "integer" },
        target_window: { type: ["integer", "null"] },
        actions: { type: "array", minItems: 1, items: actionSchema() },
        approval_proof: { type: ["string", "null"] },
      },
    }),
    functionTool("computer_exec_js", "Evaluate persistent capability-empty JavaScript against this session's computer", {
      type: "object",
      required: ["protocol_version", "task", "lease_generation", "expected_sequence", "source"],
      properties: {
        protocol_version: { const: PROTOCOL_VERSION },
        task: { type: "string" },
        lease_generation: { type: "integer" },
        expected_sequence: { type: "integer" },
        source: { type: "string" },
        approval_proof: { type: ["string", "null"] },
      },
    }),
    functionTool("computer_observe", "Read task events, approvals, and observations newer than the supplied cursors", {
      type: "object",
      required: ["task", "event_cursor", "observation_sequence"],
      properties: {
        task: { type: "string" },
        event_cursor: { type: "integer" },
        observation_sequence: { type: "integer" },
      },
    }),
  ];
  for (const [name, description] of [
    ["computer_pause", "Pause a task and release held input"],
    ["computer_stop", "Stop a task, release input, and destroy its script isolate"],
  ]) {
    tools.push(functionTool(name, description, {
      type: "object",
      required: ["task"],
      properties: { task: { type: "string" } },
    }));
  }
  tools.push(
    functionTool("computer_resume", "Resume a task with a new daemon-wide lease", {
      type: "object",
      required: ["task", "lease_duration_ms"],
      properties: { task: { type: "string" }, lease_duration_ms: { type: "integer", minimum: 1 } },
    }),
    functionTool("computer_renew", "Renew the current lease before it expires", {
      type: "object",
      required: ["task", "lease_generation", "lease_duration_ms"],
      properties: {
        task: { type: "string" },
        lease_generation: { type: "integer" },
        lease_duration_ms: { type: "integer", minimum: 1 },
      },
    }),
    functionTool("computer_approve", "Approve or deny a pending checkpoint owned by this task", {
      type: "object",
      required: ["task", "request_id", "decision"],
      properties: {
        task: { type: "string" },
        request_id: { type: "string" },
        decision: { enum: ["approve", "deny"] },
      },
    }),
  );
  return tools;
}

export async function requestNativeTool(socketPath, command) {
  if (!socketPath) throw new Error(`${SOCKET_ENV} is not set`);
  return new Promise((resolve, reject) => {
    const socket = connect(socketPath);
    const chunks = [];
    let bytes = 0;
    let settled = false;
    const finish = (callback, value) => {
      if (settled) return;
      settled = true;
      socket.destroy();
      callback(value);
    };
    socket.setTimeout(120_000, () => finish(reject, new Error("native computer tool timed out")));
    socket.on("error", (error) => finish(reject, error));
    socket.on("data", (chunk) => {
      bytes += chunk.length;
      if (bytes > MAX_RESPONSE_BYTES) {
        finish(reject, new Error("native computer tool response exceeded 64 MiB"));
        return;
      }
      chunks.push(chunk);
    });
    socket.on("end", () => {
      try {
        const response = JSON.parse(Buffer.concat(chunks).toString("utf8"));
        if (response.protocol_version !== PROTOCOL_VERSION) {
          throw new Error("native computer tool protocol version mismatch");
        }
        if (!response.ok) throw new Error(response.error || "native computer tool failed");
        finish(resolve, String(response.output ?? ""));
      } catch (error) {
        finish(reject, error);
      }
    });
    socket.on("connect", () => socket.end(`${JSON.stringify({ command })}\n`));
  });
}

function requiredString(arguments_, name) {
  const value = arguments_?.[name];
  if (typeof value !== "string" || value.length === 0) throw new Error(`${name} must be a non-empty string`);
  return value;
}

function requiredInteger(arguments_, name) {
  const value = arguments_?.[name];
  if (!Number.isSafeInteger(value) || value < 0) throw new Error(`${name} must be an unsigned integer`);
  return String(value);
}

function sleep(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function takePngs(value, images) {
  if (Array.isArray(value)) {
    for (const child of value) takePngs(child, images);
    return;
  }
  if (!value || typeof value !== "object") return;
  if (Array.isArray(value.png)) {
    const png = Buffer.from(value.png);
    if (png.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))) images.push(png);
    delete value.png;
  }
  if (Array.isArray(value.displayed_pngs)) {
    for (const bytes of value.displayed_pngs) {
      const png = Buffer.from(bytes);
      if (png.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))) images.push(png);
    }
    delete value.displayed_pngs;
  }
  for (const child of Object.values(value)) takePngs(child, images);
}

export function nativeToolResult(response) {
  let value;
  try {
    value = JSON.parse(response.trim());
  } catch {
    return { output: [{ type: "input_text", text: response }], text: response, isError: false };
  }
  const images = [];
  takePngs(value, images);
  const text = JSON.stringify(value);
  const output = [{ type: "input_text", text }];
  for (const png of images) {
    output.push({ type: "input_image", image_url: `data:image/png;base64,${png.toString("base64")}` });
  }
  return { output, text, isError: false };
}

export class NativeComputerUseClient {
  constructor({ socketPath = process.env[SOCKET_ENV], session, principal }) {
    this.socketPath = socketPath;
    this.session = Number(session);
    this.principal = String(principal || "brood-agent");
    this.ownedTasks = new Set();
    this.nextTask = 1;
    if (!Number.isSafeInteger(this.session) || this.session <= 0) throw new Error("computer-use session must be a positive integer");
    if (!this.principal.trim()) throw new Error("computer-use principal must not be empty");
  }

  tools() {
    return computerUseTools(this.session);
  }

  generatedTaskId() {
    const principal = this.principal.replaceAll(/[^A-Za-z0-9_-]/g, "-").slice(0, 48);
    return `${principal}-computer-${process.pid}-${this.nextTask++}`;
  }

  ensureOwned(task) {
    if (!this.ownedTasks.has(task)) throw new Error("task is not owned by this Brood agent turn");
  }

  command(command) {
    return requestNativeTool(this.socketPath, command);
  }

  async call(name, arguments_ = {}, callId = "computer-call") {
    if (name === "computer_windows") {
      return nativeToolResult(await this.command(["computer-use", "windows", String(this.session)]));
    }
    if (name === "computer_launch") return nativeToolResult(await this.launch(arguments_));
    let command;
    let startedTask;
    let stoppedTask;
    if (name === "computer_start") {
      const task = typeof arguments_.task === "string" && arguments_.task ? arguments_.task : this.generatedTaskId();
      const requestedSession = arguments_.session ?? arguments_.scope?.session;
      if (requestedSession !== undefined && Number(requestedSession) !== this.session) throw new Error("requested session is outside this agent pane");
      const request = {
        protocol_version: PROTOCOL_VERSION,
        task,
        principal: this.principal,
        scope: { session: this.session, windows: arguments_.windows ?? arguments_.scope?.windows ?? [] },
        policy: arguments_.policy ?? DEFAULT_POLICY,
        limits: arguments_.limits ?? DEFAULT_LIMITS,
        lease_duration_ms: arguments_.lease_duration_ms ?? DEFAULT_LEASE_DURATION_MS,
        active_window: arguments_.active_window ?? null,
      };
      command = ["computer-use", "start-request", JSON.stringify(request)];
      startedTask = task;
    } else if (name === "computer_action") {
      const task = requiredString(arguments_, "task");
      this.ensureOwned(task);
      command = ["computer-use", "native-call", JSON.stringify({
        tool_call_id: callId,
        arguments: { ...arguments_, task, target_session: this.session, call_id: callId },
      })];
    } else if (name === "computer_exec_js") {
      this.ensureOwned(requiredString(arguments_, "task"));
      command = ["computer-use", "exec-js", JSON.stringify(arguments_)];
    } else if (name === "computer_observe") {
      const task = requiredString(arguments_, "task");
      this.ensureOwned(task);
      command = ["computer-use", "observe", task, requiredInteger(arguments_, "event_cursor"), requiredInteger(arguments_, "observation_sequence")];
    } else if (name === "computer_pause" || name === "computer_stop") {
      const task = requiredString(arguments_, "task");
      this.ensureOwned(task);
      command = ["computer-use", name.slice("computer_".length), task];
      if (name === "computer_stop") stoppedTask = task;
    } else if (name === "computer_resume") {
      const task = requiredString(arguments_, "task");
      this.ensureOwned(task);
      command = ["computer-use", "resume", task, requiredInteger(arguments_, "lease_duration_ms")];
    } else if (name === "computer_renew") {
      const task = requiredString(arguments_, "task");
      this.ensureOwned(task);
      command = ["computer-use", "renew", task, requiredInteger(arguments_, "lease_generation"), requiredInteger(arguments_, "lease_duration_ms")];
    } else if (name === "computer_approve") {
      const task = requiredString(arguments_, "task");
      this.ensureOwned(task);
      command = ["computer-use", "approve-for-task", task, requiredString(arguments_, "request_id"), requiredString(arguments_, "decision"), this.principal];
    } else {
      throw new Error(`unknown native computer tool ${name}`);
    }
    const response = await this.command(command);
    if (startedTask) this.ownedTasks.add(startedTask);
    if (stoppedTask) this.ownedTasks.delete(stoppedTask);
    return nativeToolResult(response);
  }

  async launch(arguments_) {
    const command = requiredString(arguments_, "command");
    if (command.length > 256) throw new Error("launch command exceeds 256 bytes");
    const args = arguments_.args ?? [];
    if (!Array.isArray(args) || args.some((arg) => typeof arg !== "string")) throw new Error("launch args must be an array of strings");
    if (args.length > MAX_LAUNCH_ARGUMENTS) throw new Error(`launch accepts at most ${MAX_LAUNCH_ARGUMENTS} arguments`);
    if (command.length + args.reduce((total, arg) => total + arg.length, 0) > MAX_LAUNCH_ARGUMENT_BYTES) throw new Error("launch command and arguments exceed 16 KiB");
    const timeoutMs = arguments_.window_timeout_ms ?? DEFAULT_LAUNCH_TIMEOUT_MS;
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > MAX_LAUNCH_TIMEOUT_MS) throw new Error(`window timeout must be between 1 and ${MAX_LAUNCH_TIMEOUT_MS} milliseconds`);
    const response = await this.command(["spawn-in-session", String(this.session), command, ...args]);
    const pane = Number(response.split(/\s+/).find((field) => field.startsWith("pane_id="))?.slice(8));
    if (!Number.isSafeInteger(pane)) throw new Error("Brood launch response did not contain a pane ID");
    const deadline = Date.now() + timeoutMs;
    while (true) {
      const status = JSON.parse((await this.command(["hosted-child-status", String(pane)])).trim());
      if (status.visible === true) return JSON.stringify({ session: this.session, pane, command, args, state: status.state ?? null, visible: true });
      if (status.state === "exited") throw new Error(`${command} exited before creating a managed window: ${status.detail ?? "unknown exit status"}`);
      if (Date.now() >= deadline) throw new Error(`timed out waiting for ${command} to create a managed window; last status: ${JSON.stringify(status)}`);
      await sleep(25);
    }
  }

  async close() {
    const tasks = [...this.ownedTasks];
    this.ownedTasks.clear();
    await Promise.allSettled(tasks.map((task) => this.command(["computer-use", "stop", task])));
  }
}
