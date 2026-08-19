#!/usr/bin/env node

import { runAgent } from "../../agents/brood-agent/src/agent.mjs";

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));
const mcp = {
  async tools() {
    return [];
  },
  async call() {
    throw new Error("computer-use tools are disabled in repository evals");
  },
};

await runAgent(request, { mcp });
