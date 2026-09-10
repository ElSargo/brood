import { spawn } from "node:child_process";

// Brood can cancel a worker with SIGKILL. A separate supervisor owns each shell
// process group and notices the runner's IPC disconnect even when its signal
// handlers cannot run. stdin/stdout/stderr retain normal pipe semantics.
let child, stopping = false, escalation, exitCode, exitSignal;
function killGroup(signal) {
  if (!child?.pid) return false;
  try { process.kill(-child.pid, signal); return true; } catch { return false; }
}
function finish() {
  clearTimeout(escalation);
  const exit = () => process.exit(exitCode ?? 1);
  if (process.connected) process.send({ type: "exit", signal: exitSignal ?? null }, exit);
  else exit();
}
function stop() {
  if (stopping) return;
  stopping = true;
  killGroup("SIGTERM");
  // Finish before the runner's own escalation can kill this supervisor.
  escalation = setTimeout(() => { killGroup("SIGKILL"); finish(); }, 100);
}
process.on("disconnect", stop);
process.on("SIGINT", stop);
process.on("SIGTERM", stop);
process.stdin.on("error", stop);
child = spawn("bash", [process.argv[2], process.argv[3]], {
  detached: true, stdio: ["pipe", "inherit", "inherit"],
});
child.stdin.on("error", () => {});
process.stdin.pipe(child.stdin);
child.on("error", error => {
  process.stderr.write(`Start command: ${error.message}\n`);
  exitCode = 1;
  finish();
});
child.on("exit", (code, signal) => {
  exitCode = code;
  exitSignal = signal;
  // Background descendants must not escape when their shell leader exits.
  if (!stopping) {
    if (killGroup(0)) stop();
    else finish();
  }
});
