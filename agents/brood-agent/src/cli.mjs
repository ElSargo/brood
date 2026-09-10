#!/usr/bin/env node

import { runAgent, createAbortController } from "./agent.mjs";
import { configPath, loadConfig, resolveSettings } from "./config.mjs";
import { readFile, readdir, mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { stateDirectory } from "./storage.mjs";
import { importCodexLogin } from "./auth.mjs";

function verifyNativeComputerUseEnvironment() {
  if (!process.env.BROOD_AGENT_REQUIRE_DBUS_FREE) return;
  if (!process.env.BROOD_NATIVE_TOOL_SOCKET) {
    throw new Error("native computer tool socket was not inherited");
  }
  for (const variable of [
    "BROODD_DBUS_DESTINATION",
    "BROODD_DBUS_ADDRESS",
    "DBUS_SESSION_BUS_ADDRESS",
  ]) {
    if (process.env[variable]) throw new Error(`${variable} leaked into the native agent`);
  }
  process.stdout.write(`${JSON.stringify({ type: "status", text: "Native computer tools ready without DBus" })}\n`);
}

async function readRequest() {
  const chunks = [];
  let bytes = 0;
  for await (const chunk of process.stdin) {
    bytes += chunk.length;
    if (bytes > 24 * 1024 * 1024) throw new Error("Agent request exceeds 24 MiB");
    chunks.push(chunk);
  }
  const body = Buffer.concat(chunks).toString("utf8").trim();
  if (!body) throw new Error("brood-agent expected one JSON request on stdin");
  return JSON.parse(body);
}

export async function main(args = process.argv.slice(2)) {
  if (args[0] === "--help") {
    process.stdout.write("brood-agent [config init|config check|auth import-codex|sessions]\nWith no arguments, read one JSON turn request from stdin and emit JSON events.\nRequest fields: prompt, conversation_id, model, reasoning, provider, profile, session_id, agent, history, images.\nResume with conversation_id, or use /resume ID in a prompt.\n");
    return;
  }
  if (args[0] === "auth" && args[1] === "import-codex" && args.length === 2) {
    process.stdout.write(`Imported login into ${await importCodexLogin()}\n`);
    return;
  }
  if (args[0] === "config" && args.length === 2 && ["init", "check"].includes(args[1])) {
    const path = configPath();
    if (args[1] === "init") {
      await mkdir(dirname(path), { recursive: true, mode: 0o700 });
      const example = await readFile(new URL("../agent.example.json", import.meta.url));
      await writeFile(path, example, { flag: "wx", mode: 0o600 });
      process.stdout.write(`Created ${path}\n`);
    } else {
      const config = await loadConfig();
      resolveSettings(config);
      for (const profile of Object.keys(config.profiles ?? {})) resolveSettings(config, { profile });
      process.stdout.write(`Valid configuration: ${path}\n`);
    }
    return;
  }
  if (args[0] === "sessions" && args.length === 1) {
    const directory = stateDirectory();
    const entries = await readdir(directory).catch(error => { if (error.code === "ENOENT") return []; throw error; });
    for (const entry of entries.filter(entry => entry.endsWith(".json")).sort()) {
      const state = JSON.parse(await readFile(join(directory, entry), "utf8"));
      process.stdout.write(JSON.stringify({ id: state.id, workspace: state.workspace, status: state.status, updated_at: state.updated_at, model: state.preferences?.model }) + "\n");
    }
    return;
  }
  if (args.length) throw new Error("Unknown arguments; use --help");
  verifyNativeComputerUseEnvironment();
  const request = await readRequest();
  const controller = createAbortController();
  try { await runAgent(request, { signal: controller.signal }); }
  finally { controller.dispose(); }
}

try {
  await main();
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`${message}\n`);
  process.exitCode = 1;
}
