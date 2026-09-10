import { chmod, mkdir, open, readFile, rename, rm, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { randomUUID } from "node:crypto";

export function stateDirectory(env = process.env) {
  return env.BROOD_AGENT_STATE_DIR || join(env.XDG_STATE_HOME || join(env.HOME || homedir(), ".local", "state"), "brood", "agent");
}

export async function atomicJson(path, value) {
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  await chmod(dirname(path), 0o700);
  const temporary = `${path}.${randomUUID()}.tmp`;
  let file;
  try {
    file = await open(temporary, "wx", 0o600);
    await file.writeFile(`${JSON.stringify(value)}\n`);
    await file.sync();
    await file.close();
    file = undefined;
    await rename(temporary, path);
    const directory = await open(dirname(path), "r");
    try { await directory.sync(); } finally { await directory.close(); }
  } finally {
    await file?.close();
    await rm(temporary, { force: true });
  }
}

export class ConversationStore {
  constructor(directory, id = randomUUID()) {
    if (!/^[a-zA-Z0-9_-]{1,100}$/.test(id)) throw new Error("Invalid conversation_id");
    this.id = id;
    this.path = join(directory, `${id}.json`);
    this.lockPath = `${this.path}.lock`;
  }

  async acquire() {
    if (this.lock) throw new Error(`Conversation ${this.id} is already locked by this store`);
    await mkdir(dirname(this.path), { recursive: true, mode: 0o700 });
    await chmod(dirname(this.path), 0o700);
    // Never unlink this inode: flock arbitrates all contenders and the kernel
    // releases it on process exit, including crashes during lock acquisition.
    const file = await open(this.lockPath, "a", 0o600);
    try { await file.chmod(0o600); } finally { await file.close(); }
    const child = spawn("flock", ["--exclusive", "--nonblock", "--conflict-exit-code", "75", "--no-fork", this.lockPath, process.execPath, fileURLToPath(new URL("./lock-holder.mjs", import.meta.url))], { stdio: ["pipe", "pipe", "pipe"] });
    child.stdin.on("error", () => {});
    const done = new Promise(resolve => child.once("close", resolve));
    let details = "";
    child.stderr.on("data", chunk => { details = (details + chunk).slice(-2000); });
    try {
      await new Promise((resolve, reject) => {
        const timer = setTimeout(() => { child.kill("SIGKILL"); reject(new Error("Conversation lock helper did not start")); }, 5000);
        child.once("error", error => { clearTimeout(timer); reject(new Error(`Acquire conversation lock: ${error.message}; install util-linux (flock)`)); });
        child.stdout.once("data", () => { clearTimeout(timer); resolve(); });
        child.once("close", code => {
          clearTimeout(timer);
          reject(code === 75
            ? Object.assign(new Error(`Conversation ${this.id} is already in use`), { code: "CONVERSATION_BUSY" })
            : new Error(`Conversation lock helper exited (${code}): ${details.trim()}`));
        });
      });
      this.lock = { child, done };
    } catch (error) {
      child.stdin.end();
      await done;
      throw error;
    }
  }

  async read() {
    try {
      if ((await stat(this.path)).size > 128 * 1024 * 1024) throw new Error("Conversation checkpoint exceeds 128 MiB");
      const value = JSON.parse(await readFile(this.path, "utf8"));
      if (value.version !== 1 || value.id !== this.id || !Array.isArray(value.input) || !Array.isArray(value.history)) throw new Error("Invalid conversation checkpoint");
      return value;
    } catch (error) {
      if (error.code === "ENOENT") return undefined;
      throw new Error(`Read conversation ${this.id}: ${error.message}`);
    }
  }

  async save(state) {
    if (!this.lock || this.lock.child.exitCode !== null || this.lock.child.signalCode !== null) throw new Error("Conversation lock is no longer held");
    await atomicJson(this.path, { ...state, version: 1, id: this.id, updated_at: new Date().toISOString() });
  }

  async close() {
    if (!this.lock) return;
    const lock = this.lock;
    this.lock = undefined;
    lock.child.stdin.end();
    await lock.done;
  }
}

export function recoverInterruptedCalls(input) {
  const results = new Set(input.filter(item => item.type === "function_call_output").map(item => item.call_id));
  const missing = input.filter(item => item.type === "function_call" && !results.has(item.call_id));
  for (const call of missing) input.push({ type: "function_call_output", call_id: call.call_id, output: "Execution was interrupted. This action may have run partially or completely; its result was not checkpointed. Inspect actual state before retrying. Do not assume it failed or repeat it blindly." });
  return missing.length;
}
