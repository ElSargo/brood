import { fork } from "node:child_process";
import { realpath, mkdir } from "node:fs/promises";
import { createWriteStream } from "node:fs";
import { isAbsolute, relative, resolve, join } from "node:path";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { abortable } from "./transport.mjs";

export async function commandWorkdir(value, workspace = process.cwd()) {
  const root = await realpath(resolve(workspace));
  const candidate = await realpath(resolve(root, String(value || ".")));
  const offset = relative(root, candidate);
  if (offset === ".." || offset.startsWith("../") || isAbsolute(offset)) throw new Error("workdir must remain inside the session workspace");
  return candidate;
}

export function commandCapture(maximum) {
  const marker = Buffer.from("\n… output truncated; showing beginning and end …\n");
  const headLimit = Math.ceil((maximum - marker.length) / 2);
  const tailLimit = Math.floor((maximum - marker.length) / 2);
  let head = Buffer.alloc(0), tail = Buffer.alloc(0), total = 0;
  return {
    push(chunk) {
      total += chunk.length;
      if (head.length < maximum) head = Buffer.concat([head, chunk.subarray(0, maximum - head.length)]);
      tail = chunk.length >= tailLimit ? chunk.subarray(chunk.length - tailLimit) : Buffer.concat([tail, chunk]).subarray(-tailLimit);
    },
    result() {
      const truncated = total > maximum;
      return { text: (truncated ? Buffer.concat([head.subarray(0, headLimit), marker, tail]) : head).toString("utf8"), truncated };
    },
    clear() { head = Buffer.alloc(0); tail = Buffer.alloc(0); total = 0; },
  };
}

function integer(value, fallback, min, max, name) {
  if (value === undefined) return fallback;
  if (!Number.isSafeInteger(value) || value < min || value > max) throw new Error(`${name} must be an integer between ${min} and ${max}`);
  return value;
}

export class CommandSessions {
  constructor({ workspace = process.cwd(), signal, outputBytes = 32768, artifactDirectory } = {}) {
    Object.assign(this, { workspace, signal, outputBytes, artifactDirectory });
    this.sessions = new Map();
    this.abort = () => { for (const session of this.sessions.values()) this.terminate(session, "aborted"); };
    signal?.addEventListener("abort", this.abort, { once: true });
  }

  terminate(session, reason) {
    session[reason] = true;
    if (session.terminated) return;
    session.terminated = true;
    const kill = sig => {
      try {
        if (process.platform !== "win32" && session.child.pid) process.kill(-session.child.pid, sig);
        else session.child.kill(sig);
      } catch {}
    };
    kill("SIGTERM");
    session.force = setTimeout(() => kill("SIGKILL"), 250);
  }

  async start(args, signal = this.signal) {
    signal?.throwIfAborted();
    this.signal?.throwIfAborted();
    if (this.sessions.size >= 16) throw new Error("At most 16 commands may be active; poll or terminate existing sessions first");
    const command = args.command ?? args.cmd;
    if (typeof command !== "string" || !command.trim()) throw new Error("command must be a nonempty string");
    const timeout = integer(args.timeout_ms, 120000, 1, 3600000, "timeout_ms");
    const maximum = integer(args.max_output_bytes, this.outputBytes, 1024, 1048576, "max_output_bytes");
    const cwd = await commandWorkdir(args.workdir, this.workspace);
    signal?.throwIfAborted();
    this.signal?.throwIfAborted();
    const id = randomUUID();
    const files = {};
    const streams = [];
    if (this.artifactDirectory) {
      await mkdir(this.artifactDirectory, { recursive: true, mode: 0o700 });
      for (const name of ["stdout", "stderr"]) {
        files[name] = join(this.artifactDirectory, `${id}.${name}.log`);
        const stream = createWriteStream(files[name], { flags: "wx", mode: 0o600 });
        await once(stream, "open");
        streams.push(stream);
      }
    }
    const child = fork(new URL("./command-supervisor.mjs", import.meta.url), [args.login === true ? "-lc" : "-c", command], {
      cwd, stdio: ["pipe", "pipe", "pipe", "ipc"], detached: true, execArgv: [],
    });
    const session = { id, child, cwd, started: performance.now(), stdout: commandCapture(maximum), stderr: commandCapture(maximum), files, streams };
    this.sessions.set(id, session);
    child.stdin.on("error", () => {});
    child.on("message", message => { if (message.type === "exit") session.command_signal = message.signal; });
    for (const [index, name] of ["stdout", "stderr"].entries()) {
      let logged = 0;
      child[name].on("data", chunk => {
        session[name].push(chunk);
        // Full output remains available for ordinary commands; stop disk growth
        // at 32 MiB per stream and report that bound explicitly.
        if (streams[index]) {
          const remaining = Math.max(0, 32 * 1024 * 1024 - logged);
          if (remaining) {
            if (!streams[index].write(chunk.subarray(0, remaining))) child[name].pause();
          }
          logged += chunk.length;
          if (logged > 32 * 1024 * 1024) session.artifact_truncated = true;
        }
      });
      streams[index]?.on("drain", () => child[name].resume());
      streams[index]?.on("error", error => { session.error = `Capture output: ${error.message}`; this.terminate(session, "aborted"); child[name].resume(); });
    }
    child.on("error", error => { session.error = error.message; });
    session.done = new Promise(resolveDone => child.once("close", async (code, killedBy) => {
      session.exit_code = code;
      session.signal = session.command_signal ?? killedBy;
      clearTimeout(session.timer);
      // Keep escalation alive after the leader exits: descendants may ignore TERM.
      if (session.force) await new Promise(resolve => setTimeout(resolve, 260));
      await Promise.all(streams.map(stream => new Promise(resolve => { if (stream.destroyed) resolve(); else stream.end(resolve); })));
      session.closed = true;
      resolveDone();
    }));
    session.timer = setTimeout(() => this.terminate(session, "timed_out"), timeout);
    if (this.signal?.aborted) this.terminate(session, "aborted");
    return session;
  }

  async read(session, waitMs, signal = this.signal) {
    let timer;
    try { await abortable(Promise.race([session.done, new Promise(resolve => { timer = setTimeout(resolve, waitMs); })]), signal); }
    finally { clearTimeout(timer); }
    const stdout = session.stdout.result(), stderr = session.stderr.result();
    session.stdout.clear(); session.stderr.clear();
    const result = {
      stdout: stdout.text, stderr: stderr.text, exit_code: session.closed ? session.exit_code : null,
      signal: session.signal ?? null, timed_out: !!session.timed_out, aborted: !!session.aborted,
      duration_ms: Math.round(performance.now() - session.started), workdir: session.cwd,
      truncated: stdout.truncated || stderr.truncated,
      ...(session.closed ? {} : { session_id: session.id }),
      ...(Object.keys(session.files).length ? { output_files: session.files, artifact_truncated: !!session.artifact_truncated } : {}),
      ...(session.error ? { error: session.error } : {}),
    };
    if (session.closed) this.sessions.delete(session.id);
    return { content: JSON.stringify(result), isError: !!(session.error || session.timed_out || session.aborted || (session.closed && session.exit_code !== 0)), summary: `${session.closed ? `exit ${session.exit_code ?? session.signal}` : `running · session ${session.id}`} · ${result.duration_ms} ms${result.truncated ? " · output truncated" : ""}` };
  }

  async exec(args, signal = this.signal) {
    const wait = integer(args.yield_time_ms, 10000, 0, 30000, "yield_time_ms");
    const session = await this.start(args, signal);
    const abort = () => this.terminate(session, "aborted");
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    try { return await this.read(session, wait, signal); }
    finally { signal?.removeEventListener("abort", abort); }
  }

  async write(args, signal = this.signal) {
    signal?.throwIfAborted();
    this.signal?.throwIfAborted();
    const session = this.sessions.get(args.session_id);
    if (!session) throw new Error("Unknown command session; it may have finished or belong to a previous turn");
    const wait = integer(args.yield_time_ms, 1000, 0, 30000, "yield_time_ms");
    if (args.terminate) this.terminate(session, "aborted");
    else if (args.chars !== undefined) {
      if (typeof args.chars !== "string") throw new Error("chars must be a string");
      session.child.stdin.write(args.chars);
    }
    if (args.close_stdin) session.child.stdin.end();
    const abort = () => this.terminate(session, "aborted");
    signal?.addEventListener("abort", abort, { once: true });
    try { return await this.read(session, wait, signal); }
    finally { signal?.removeEventListener("abort", abort); }
  }

  async close() {
    this.signal?.removeEventListener("abort", this.abort);
    for (const session of this.sessions.values()) this.terminate(session, "aborted");
    await Promise.all([...this.sessions.values()].map(session => session.done));
    this.sessions.clear();
  }
}

export async function runExecCommand(args, signal, workspace = process.cwd()) {
  const commands = new CommandSessions({ workspace, signal, outputBytes: 1048576 });
  try {
    const session = await commands.start({ ...args, timeout_ms: args.timeout_ms ?? 30000 });
    session.child.stdin.end();
    await abortable(session.done, signal);
    return await commands.read(session, 0);
  } finally { await commands.close(); }
}
