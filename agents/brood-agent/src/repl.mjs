import { createConnection } from "node:net";
import { spawn } from "node:child_process";
import { mkdir, chmod, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { waitForRetry } from "./transport.mjs";

export class CodeRepl {
  constructor({ id, workspace, signal, callTool, outputBytes = 32768 }) {
    Object.assign(this, { workspace, signal, callTool, outputBytes });
    const directory = join(tmpdir(), `brood-repl-${process.getuid?.() ?? "user"}`);
    this.directory = directory;
    this.path = join(directory, createHash("sha256").update(`${workspace}\0${id}`).digest("hex").slice(0, 32) + ".sock");
  }

  async connect() {
    this.signal?.throwIfAborted();
    return new Promise((resolve, reject) => {
      const socket = createConnection(this.path);
      socket.once("error", reject);
      socket.once("connect", () => { socket.removeListener("error", reject); socket.setEncoding("utf8"); resolve(socket); });
    });
  }

  async ensure() {
    try { return await this.connect(); }
    catch (error) { if (!["ENOENT", "ECONNREFUSED"].includes(error.code)) throw error; }
    await mkdir(this.directory, { recursive: true, mode: 0o700 });
    await chmod(this.directory, 0o700);
    await rm(this.path, { force: true });
    const child = spawn(process.execPath, [new URL("./repl-server.mjs", import.meta.url).pathname, this.path, this.workspace], { detached: true, stdio: "ignore" });
    let failed;
    child.on("error", error => { failed = error; });
    child.unref();
    for (let attempt = 0; attempt < 100; attempt++) {
      await waitForRetry(20, this.signal);
      if (failed) throw failed;
      try { return await this.connect(); }
      catch (error) { if (!["ENOENT", "ECONNREFUSED"].includes(error.code)) throw error; }
    }
    throw new Error("REPL did not start");
  }

  async run(args = {}, operation = "eval") {
    const timeout = args.timeout_ms ?? 30000;
    if (!Number.isSafeInteger(timeout) || timeout < 1 || timeout > 120000) throw new Error("REPL timeout_ms must be between 1 and 120000");
    if (operation === "eval" && (typeof args.code !== "string" || Buffer.byteLength(args.code) > 1024 * 1024)) throw new Error("REPL code must be a string of at most 1 MiB");
    if (this.busy) throw new Error("REPL is busy; await the current cell");
    this.busy = true;
    const cells = new AbortController();
    const abort = () => cells.abort(this.signal.reason);
    this.signal?.addEventListener("abort", abort, { once: true });
    let socket;
    try {
      socket = await this.ensure();
      this.signal?.throwIfAborted();
      return await new Promise((resolve, reject) => {
        let pending = "", settled = false;
        const stop = () => { socket.destroy(); reject(this.signal.reason ?? new Error("REPL cancelled")); };
        this.signal?.addEventListener("abort", stop, { once: true });
        socket.setTimeout(timeout + 5000, () => { socket.destroy(); reject(new Error("REPL connection timed out")); });
        socket.on("error", reject);
        socket.on("close", () => {
          this.signal?.removeEventListener("abort", stop);
          if (!settled) reject(new Error("REPL connection closed before returning a result; inspect effects before retrying"));
        });
        socket.on("data", chunk => {
          pending += chunk.toString("utf8");
          if (Buffer.byteLength(pending) > 4 * 1024 * 1024) { socket.destroy(); return; }
          let newline;
          while ((newline = pending.indexOf("\n")) >= 0) {
            let message;
            try { message = JSON.parse(pending.slice(0, newline)); } catch (error) { reject(error); socket.destroy(); return; }
            pending = pending.slice(newline + 1);
            if (message.type === "result") {
              settled = true;
              const restarted = this.generation && this.generation !== message.generation;
              this.generation = message.generation;
              resolve({ content: JSON.stringify({ ...message, ...(restarted ? { restarted: true } : {}) }), isError: !!message.isError, summary: message.error ?? "JavaScript cell completed" });
              socket.end();
            } else if (message.type === "tool") {
              Promise.resolve().then(() => {
                if (["js_repl", "js_repl_reset"].includes(message.name)) throw new Error("REPL tools cannot call themselves");
                return this.callTool(message.name, message.args, cells.signal);
              }).then(result => {
                if (!socket.destroyed) socket.write(JSON.stringify({ type: "tool_result", call: message.call, result }) + "\n");
              }, error => {
                if (!socket.destroyed) socket.write(JSON.stringify({ type: "tool_result", call: message.call, error: error.message }) + "\n");
              });
            }
          }
        });
        socket.write(JSON.stringify({ type: operation, code: args.code, timeout_ms: timeout, max_output_bytes: this.outputBytes }) + "\n");
      });
    } finally {
      this.busy = false;
      this.signal?.removeEventListener("abort", abort);
      cells.abort(new Error("REPL cell ended"));
      socket?.destroy();
    }
  }
}
