import { createServer } from "node:net";
import { fork } from "node:child_process";
import { randomUUID } from "node:crypto";
import { rm, chmod } from "node:fs/promises";
import { commandCapture } from "./commands.mjs";

const [socketPath, workspace] = process.argv.slice(2);
let kernel, generation, active, idle;
function killKernel() {
  const child = kernel;
  kernel = undefined;
  if (child?.pid) {
    try { process.kill(-child.pid, "SIGKILL"); } catch {}
  }
}
function finish(error, reset = false) {
  if (!active) return;
  const cell = active;
  active = undefined;
  clearTimeout(cell.timer);
  if (reset) killKernel();
  cell.socket.end(JSON.stringify({ type: "result", generation, ...cell.output.result(), isError: !!error, ...(error ? { error } : {}), ...(reset ? { reset: true } : {}) }) + "\n");
  armIdle();
}
function armIdle() {
  clearTimeout(idle);
  idle = setTimeout(shutdown, 30 * 60 * 1000);
}
function startKernel() {
  if (kernel) return;
  generation = randomUUID();
  const child = fork(new URL("./repl-kernel.mjs", import.meta.url), [], { cwd: workspace, detached: true, stdio: ["ignore", "pipe", "pipe", "ipc"] });
  kernel = child;
  for (const stream of [child.stdout, child.stderr]) stream.on("data", chunk => active?.output.push(chunk));
  child.on("message", message => {
    if (child !== kernel || !active || message.id !== active.id) return;
    if (message.type === "output") active.output.push(Buffer.from(message.text));
    else if (message.type === "tool") active.socket.write(JSON.stringify(message) + "\n");
    else if (message.type === "done") finish(message.error, message.reset);
  });
  child.on("error", error => { if (child === kernel) finish(`REPL process failed: ${error.message}`, true); });
  child.on("exit", () => {
    if (child === kernel) { kernel = undefined; finish("REPL process exited; variables were reset", true); }
  });
}
async function shutdown() {
  clearTimeout(idle);
  finish("REPL closed", true);
  clearTimeout(idle);
  killKernel();
  server.close();
  await rm(socketPath, { force: true });
  process.exit(0);
}
const server = createServer(socket => {
  socket.setEncoding("utf8");
  let pending = "";
  socket.setTimeout(125000, () => socket.destroy());
  socket.on("error", () => {});
  socket.on("data", chunk => {
    pending += chunk.toString("utf8");
    if (Buffer.byteLength(pending) > 4 * 1024 * 1024) { socket.destroy(); return; }
    let newline;
    while ((newline = pending.indexOf("\n")) >= 0) {
      const line = pending.slice(0, newline); pending = pending.slice(newline + 1);
      let request;
      try { request = JSON.parse(line); } catch { socket.destroy(); return; }
      if (request.type === "tool_result") {
        if (active?.socket === socket) kernel?.send(request);
        continue;
      }
      if (active) { socket.end(JSON.stringify({ type: "result", isError: true, error: "REPL is busy; await the current cell" }) + "\n"); return; }
      if (request.type === "close") { socket.end('{"type":"result","text":"REPL closed"}\n'); shutdown(); return; }
      if (request.type === "reset") {
        killKernel();
        socket.end('{"type":"result","text":"REPL reset; variables cleared","reset":true}\n');
        armIdle();
        return;
      }
      if (request.type !== "eval" || typeof request.code !== "string" || !Number.isSafeInteger(request.timeout_ms) || request.timeout_ms < 1 || request.timeout_ms > 120000 || !Number.isSafeInteger(request.max_output_bytes) || request.max_output_bytes < 1024 || request.max_output_bytes > 1048576) { socket.destroy(); return; }
      clearTimeout(idle);
      startKernel();
      active = { socket, id: randomUUID(), output: commandCapture(request.max_output_bytes), timer: setTimeout(() => finish("REPL timed out; variables were reset", true), request.timeout_ms) };
      kernel.send({ type: "eval", id: active.id, code: request.code });
    }
  });
  socket.on("close", () => { if (active?.socket === socket) finish("REPL cancelled; variables were reset", true); });
});
server.on("error", error => { process.stderr.write(error.message + "\n"); process.exit(1); });
server.listen(socketPath, async () => { await chmod(socketPath, 0o600); armIdle(); });
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
