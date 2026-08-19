#!/usr/bin/env node

import { runAgent, createAbortController } from "./agent.mjs";

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
  for await (const chunk of process.stdin) chunks.push(chunk);
  const body = Buffer.concat(chunks).toString("utf8").trim();
  if (!body) throw new Error("brood-agent expected one JSON request on stdin");
  return JSON.parse(body);
}

try {
  verifyNativeComputerUseEnvironment();
  const request = await readRequest();
  const controller = createAbortController();
  await runAgent(request, { signal: controller.signal });
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`${message}\n`);
  process.exitCode = 1;
}
