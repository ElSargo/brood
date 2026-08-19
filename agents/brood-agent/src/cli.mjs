#!/usr/bin/env node

import { runAgent, createAbortController } from "./agent.mjs";

async function readRequest() {
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  const body = Buffer.concat(chunks).toString("utf8").trim();
  if (!body) throw new Error("brood-agent expected one JSON request on stdin");
  return JSON.parse(body);
}

try {
  const request = await readRequest();
  const controller = createAbortController();
  await runAgent(request, { signal: controller.signal });
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`${message}\n`);
  process.exitCode = 1;
}
