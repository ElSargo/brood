#!/usr/bin/env node

import { spawn } from "node:child_process";
import {
  appendFile,
  mkdir,
  readFile,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "../..");
const DEFAULT_CACHE = join(homedir(), ".cache", "brood-agent-evals");
const DATASET_URL = "https://datasets-server.huggingface.co/rows?dataset=SWE-bench%2FSWE-bench_Verified&config=default&split=test";
const MODEL = "gpt-5.6-luna";
const REASONING = "medium";

function usage() {
  process.stderr.write(
    "usage: run-swebench.mjs <brood|codex> <run-id> [instance-id ...]\n",
  );
  process.exit(2);
}

async function exists(path) {
  try {
    await stat(path);
    return true;
  } catch (error) {
    if (error?.code === "ENOENT") return false;
    throw error;
  }
}

async function run(command, args, options = {}) {
  const startedAt = Date.now();
  const child = spawn(command, args, {
    cwd: options.cwd,
    env: options.env ?? process.env,
    stdio: [options.input === undefined ? "ignore" : "pipe", "pipe", "pipe"],
  });
  if (options.input !== undefined) child.stdin.end(options.input);
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => {
    const text = chunk.toString("utf8");
    stdout += text;
    if (options.stdoutPath) appendFile(options.stdoutPath, text).catch(() => {});
  });
  child.stderr.on("data", (chunk) => {
    const text = chunk.toString("utf8");
    stderr += text;
    if (options.stderrPath) appendFile(options.stderrPath, text).catch(() => {});
  });
  const timeout = options.timeoutMs && setTimeout(() => child.kill("SIGTERM"), options.timeoutMs);
  const result = await new Promise((resolveResult, reject) => {
    child.once("error", reject);
    child.once("exit", (code, signal) => resolveResult({ code, signal }));
  });
  if (timeout) clearTimeout(timeout);
  return { ...result, stdout, stderr, duration_ms: Date.now() - startedAt };
}

async function loadDataset(cacheRoot) {
  const path = join(cacheRoot, "data", "swebench-verified.json");
  if (await exists(path)) return JSON.parse(await readFile(path, "utf8"));
  const rows = [];
  for (let offset = 0; offset < 500; offset += 100) {
    const response = await fetch(`${DATASET_URL}&offset=${offset}&length=100`);
    if (!response.ok) throw new Error(`download dataset rows: HTTP ${response.status}`);
    const page = await response.json();
    rows.push(...page.rows.map((entry) => entry.row));
  }
  if (rows.length !== 500) throw new Error(`expected 500 dataset rows, got ${rows.length}`);
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, `${JSON.stringify(rows)}\n`);
  return rows;
}

async function ensureMirror(record, cacheRoot) {
  const mirror = join(cacheRoot, "repos", `${record.repo.replaceAll("/", "__")}.git`);
  await mkdir(dirname(mirror), { recursive: true });
  if (!(await exists(mirror))) {
    const cloned = await run("git", ["clone", "--mirror", `https://github.com/${record.repo}.git`, mirror]);
    if (cloned.code !== 0) throw new Error(`clone ${record.repo}: ${cloned.stderr}`);
  }
  const hasCommit = await run("git", ["cat-file", "-e", `${record.base_commit}^{commit}`], { cwd: mirror });
  if (hasCommit.code !== 0) {
    const fetched = await run("git", ["fetch", "origin", record.base_commit], { cwd: mirror });
    if (fetched.code !== 0) throw new Error(`fetch ${record.instance_id}: ${fetched.stderr}`);
  }
  return mirror;
}

async function prepareWorkspace(record, runRoot, cacheRoot) {
  const workspace = join(runRoot, record.instance_id, "repo");
  await rm(workspace, { recursive: true, force: true });
  await mkdir(dirname(workspace), { recursive: true });
  const mirror = await ensureMirror(record, cacheRoot);
  const cloned = await run("git", ["clone", "--no-hardlinks", mirror, workspace]);
  if (cloned.code !== 0) throw new Error(`create workspace: ${cloned.stderr}`);
  const checkedOut = await run("git", ["checkout", "--detach", record.base_commit], { cwd: workspace });
  if (checkedOut.code !== 0) throw new Error(`checkout ${record.instance_id}: ${checkedOut.stderr}`);
  return workspace;
}

function taskPrompt(record) {
  return `Fix the repository issue below. Work directly in the current repository. Inspect the relevant code, implement a complete minimal fix, and run focused tests when practical. Do not use the network or look up the upstream solution. Do not merely describe a patch: make the changes in the working tree.\n\n${record.problem_statement}`;
}

async function runBrood(record, workspace, logDir) {
  return run(
    "node",
    [join(HERE, "brood-driver.mjs")],
    {
      cwd: workspace,
      input: JSON.stringify({
        session_id: `swebench-${record.instance_id}`,
        agent: "brood-swebench",
        prompt: taskPrompt(record),
        model: MODEL,
        reasoning: REASONING,
        history: [],
      }),
      stdoutPath: join(logDir, "agent.jsonl"),
      stderrPath: join(logDir, "agent.stderr"),
      timeoutMs: 30 * 60 * 1000,
    },
  );
}

async function runCodex(record, workspace, logDir) {
  return run(
    "codex",
    [
      "exec",
      "--ephemeral",
      "--ignore-user-config",
      "--model",
      MODEL,
      "--config",
      `model_reasoning_effort=\"${REASONING}\"`,
      "--config",
      'model_verbosity="low"',
      "--config",
      'approval_policy="never"',
      "--sandbox",
      "workspace-write",
      "--cd",
      workspace,
      "--json",
      "-",
    ],
    {
      cwd: workspace,
      input: taskPrompt(record),
      stdoutPath: join(logDir, "agent.jsonl"),
      stderrPath: join(logDir, "agent.stderr"),
      timeoutMs: 30 * 60 * 1000,
    },
  );
}

async function capturePatch(workspace) {
  const untracked = await run("git", ["ls-files", "--others", "--exclude-standard", "-z"], { cwd: workspace });
  if (untracked.stdout) {
    const intent = await run(
      "git",
      ["add", "-N", "--pathspec-from-file=-", "--pathspec-file-nul"],
      { cwd: workspace, input: untracked.stdout },
    );
    if (intent.code !== 0) throw new Error(`stage intent-to-add: ${intent.stderr}`);
  }
  const diff = await run("git", ["diff", "--binary", "--no-ext-diff"], { cwd: workspace });
  if (diff.code !== 0) throw new Error(`capture patch: ${diff.stderr}`);
  return diff.stdout;
}

function parseBroodUsage(output) {
  for (const line of output.trim().split("\n").reverse()) {
    try {
      const event = JSON.parse(line);
      if (event.type === "result") return event.usage;
    } catch {}
  }
}

function parseCodexUsage(output) {
  for (const line of output.trim().split("\n").reverse()) {
    try {
      const event = JSON.parse(line);
      if (event.type === "turn.completed") return event.usage;
    } catch {}
  }
}

const [agent, runId, ...requestedIds] = process.argv.slice(2);
if (!new Set(["brood", "codex"]).has(agent) || !runId) usage();
const cacheRoot = process.env.BROOD_AGENT_EVAL_CACHE || DEFAULT_CACHE;
const manifest = JSON.parse(await readFile(join(HERE, "sample.json"), "utf8"));
const ids = requestedIds.length > 0 ? requestedIds : manifest.sample;
const rows = await loadDataset(cacheRoot);
const byId = new Map(rows.map((row) => [row.instance_id, row]));
const runRoot = join(cacheRoot, "runs", runId, agent);
const predictionsPath = join(cacheRoot, "runs", runId, `${agent}-predictions.jsonl`);
await mkdir(runRoot, { recursive: true });
await writeFile(predictionsPath, "");

for (const [index, instanceId] of ids.entries()) {
  const record = byId.get(instanceId);
  if (!record) throw new Error(`unknown instance ${instanceId}`);
  const logDir = join(runRoot, record.instance_id);
  await mkdir(logDir, { recursive: true });
  await writeFile(join(logDir, "task.json"), `${JSON.stringify({
    instance_id: record.instance_id,
    repo: record.repo,
    base_commit: record.base_commit,
    difficulty: record.difficulty,
  }, null, 2)}\n`);
  process.stderr.write(`[${index + 1}/${ids.length}] ${agent} ${record.instance_id}\n`);
  const workspace = await prepareWorkspace(record, runRoot, cacheRoot);
  const result = agent === "brood"
    ? await runBrood(record, workspace, logDir)
    : await runCodex(record, workspace, logDir);
  const patch = await capturePatch(workspace);
  const summary = {
    instance_id: record.instance_id,
    agent,
    model: MODEL,
    reasoning: REASONING,
    exit_code: result.code,
    signal: result.signal,
    duration_ms: result.duration_ms,
    patch_bytes: Buffer.byteLength(patch),
    usage: agent === "brood" ? parseBroodUsage(result.stdout) : parseCodexUsage(result.stdout),
  };
  await writeFile(join(logDir, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`);
  await writeFile(join(logDir, "model.patch"), patch);
  await appendFile(predictionsPath, `${JSON.stringify({
    instance_id: record.instance_id,
    model_name_or_path: `${agent}-${MODEL}-${REASONING}`,
    model_patch: patch,
  })}\n`);
  process.stderr.write(`${JSON.stringify(summary)}\n`);
}

process.stdout.write(`${predictionsPath}\n`);
