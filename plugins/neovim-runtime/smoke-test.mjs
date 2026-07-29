#!/usr/bin/env node

import {
  mkdtempSync,
  mkdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";

const MAX_LINE_BYTES = 12 * 1024;
const TIMEOUT_MS = 60_000;
const MARKER = "config-visible:data-visible:bridge-visible";

function parseArguments(argv) {
  const options = {};
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    const value = argv[index + 1];
    if (argument === "--runner") {
      options.runner = resolve(value);
      index += 1;
    } else if (argument === "--module-dir") {
      options.moduleDirectory = resolve(value);
      index += 1;
    } else {
      throw new Error(`unknown argument: ${argument}`);
    }
  }
  if (!options.runner || !options.moduleDirectory) {
    throw new Error("--runner and --module-dir are required");
  }
  return options;
}

function runTruncatedEofProbe(options) {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(
      process.execPath,
      [
        options.runner,
        "--module-dir",
        options.moduleDirectory,
        "--config-dir",
        "/nonexistent/brood-neovim-smoke-config",
        "--data-dir",
        "/nonexistent/brood-neovim-smoke-data",
      ],
      { stdio: ["pipe", "pipe", "pipe"] },
    );
    let stdout = Buffer.alloc(0);
    let stderr = "";
    const messages = [];
    let settled = false;
    const timeout = setTimeout(() => {
      if (settled) return;
      settled = true;
      child.kill("SIGKILL");
      rejectPromise(
        new Error(`truncated EOF probe timed out; stderr:\n${stderr.slice(-4000)}`),
      );
    }, TIMEOUT_MS);

    child.stdout.on("data", (chunk) => {
      stdout = Buffer.concat([stdout, chunk]);
      while (true) {
        const newline = stdout.indexOf(0x0a);
        if (newline === -1) break;
        const line = stdout.subarray(0, newline);
        stdout = stdout.subarray(newline + 1);
        if (line.byteLength > 0) {
          messages.push(JSON.parse(line.toString("utf8")));
        }
      }
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk.toString("utf8");
    });
    child.once("error", (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      rejectPromise(error);
    });
    child.once("close", (code, signal) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      const invalidEof = messages.some(
        (message) =>
          message.type === "error" &&
          message.code === "invalid-input" &&
          String(message.message).includes("unterminated JSONL record"),
      );
      const cleanExit = messages.some(
        (message) => message.type === "exit" && message.status === 0,
      );
      if (code !== 0 || signal !== null || !invalidEof || !cleanExit) {
        rejectPromise(
          new Error(
            `truncated EOF probe failed: code=${code} signal=${signal} invalid=${invalidEof} exit=${cleanExit}; stderr:\n${stderr.slice(-4000)}`,
          ),
        );
        return;
      }
      resolvePromise();
    });
    child.stdin.end('{"v":1,"type":"truncated"');
  });
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  const temporary = mkdtempSync(join(tmpdir(), "brood-neovim-wasm-smoke-"));
  const config = join(temporary, "config");
  const data = join(temporary, "data");
  mkdirSync(config, { recursive: true });
  mkdirSync(data, { recursive: true });
  writeFileSync(
    join(data, "smoke-plugin.lua"),
    "return 'data-visible'\n",
  );
  writeFileSync(
    join(config, "init.lua"),
    [
      "local data_marker = dofile(vim.fn.stdpath('data') .. '/smoke-plugin.lua')",
      "local bridge_marker = type(vim.brood) == 'table' and 'bridge-visible' or 'bridge-missing'",
      "vim.g.brood_config_marker = 'config-visible:' .. data_marker .. ':' .. bridge_marker",
      "vim.cmd('filetype on')",
      "vim.api.nvim_create_autocmd('BufReadPost', {",
      "  pattern = '*.lua',",
      "  callback = function(event) vim.b[event.buf].brood_read_post = true end,",
      "})",
      "vim.api.nvim_create_autocmd('FileType', {",
      "  pattern = 'lua',",
      "  callback = function(event) vim.b[event.buf].brood_filetype_event = event.match end,",
      "})",
      "vim.api.nvim_create_autocmd('BufWritePre', {",
      "  pattern = '*.lua',",
      "  callback = function(event) vim.b[event.buf].brood_write_pre = true end,",
      "})",
      "vim.api.nvim_create_autocmd('BufWritePost', {",
      "  pattern = '*.lua',",
      "  callback = function(event) vim.b[event.buf].brood_write_post = true end,",
      "})",
      "for index = 1, 64 do",
      "  pcall(vim.brood.send, { type = 'action', action = 'startup-pressure-' .. index })",
      "end",
      "vim.api.nvim_create_autocmd('BufEnter', {",
      "  callback = function(event)",
      "    local name = vim.api.nvim_buf_get_name(event.buf)",
      "    if vim.fn.fnamemodify(name, ':t') == 'demo.lua' then",
      "      for index = 1, 64 do",
      "        pcall(vim.brood.send, { type = 'action', action = 'open-pressure-' .. index })",
      "      end",
      "      local bridge = require('brood_bridge')",
      "      bridge.request_save(event.buf)",
      "      for _ = 1, 11 do",
      "        bridge.request_save(event.buf)",
      "      end",
      "    end",
      "  end,",
      "})",
      "",
    ].join("\n"),
  );

  const child = spawn(
    process.execPath,
    [
      options.runner,
      "--module-dir",
      options.moduleDirectory,
      "--config-dir",
      config,
      "--data-dir",
      data,
      "--columns",
      "72",
      "--rows",
      "18",
    ],
    {
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  let stdout = Buffer.alloc(0);
  let stderr = "";
  const messages = [];
  const waiters = [];

  function receive(message) {
    messages.push(message);
    for (const waiter of [...waiters]) {
      if (waiter.predicate(message)) {
        waiters.splice(waiters.indexOf(waiter), 1);
        waiter.resolve(message);
      }
    }
  }

  child.stdout.on("data", (chunk) => {
    stdout = Buffer.concat([stdout, chunk]);
    while (true) {
      const newline = stdout.indexOf(0x0a);
      if (newline === -1) break;
      const line = stdout.subarray(0, newline);
      stdout = stdout.subarray(newline + 1);
      if (line.byteLength === 0) continue;
      if (line.byteLength > MAX_LINE_BYTES) {
        throw new Error(`runner emitted ${line.byteLength}-byte JSONL line`);
      }
      receive(JSON.parse(line.toString("utf8")));
    }
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk.toString("utf8");
  });

  function send(message) {
    const line = `${JSON.stringify({ v: 1, ...message })}\n`;
    if (Buffer.byteLength(line) > MAX_LINE_BYTES) {
      throw new Error("smoke input exceeded the runner line limit");
    }
    child.stdin.write(line);
  }

  function waitFor(predicate, description) {
    const existing = messages.find(predicate);
    if (existing) return Promise.resolve(existing);
    return new Promise((resolvePromise, rejectPromise) => {
      const timeout = setTimeout(() => {
        const index = waiters.indexOf(waiter);
        if (index !== -1) waiters.splice(index, 1);
        const recent = messages.slice(-32).map((message) => ({
          type: message.type,
          action: message.action,
          code: message.code,
          request_id: message.request_id,
          ok: message.ok,
          revision: message.revision,
          message: message.message,
        }));
        rejectPromise(
          new Error(
            `timed out waiting for ${description}; recent=${JSON.stringify(recent)}; stderr:\n${stderr.slice(-4000)}`,
          ),
        );
      }, TIMEOUT_MS);
      const waiter = {
        predicate,
        resolve(value) {
          clearTimeout(timeout);
          resolvePromise(value);
        },
      };
      waiters.push(waiter);
    });
  }

  function waitForCleanExit() {
    if (child.exitCode !== null || child.signalCode !== null) {
      if (child.exitCode === 0 && child.signalCode === null) {
        return Promise.resolve();
      }
      return Promise.reject(
        new Error(`runner exited via ${child.signalCode ?? child.exitCode}`),
      );
    }
    return new Promise((resolvePromise, rejectPromise) => {
      const timeout = setTimeout(() => {
        child.removeListener("exit", onExit);
        rejectPromise(
          new Error(
            `runner did not exit after shutdown; stderr:\n${stderr.slice(-4000)}`,
          ),
        );
      }, TIMEOUT_MS);
      function onExit(code, signal) {
        clearTimeout(timeout);
        if (code === 0 && signal === null) {
          resolvePromise();
        } else {
          rejectPromise(new Error(`runner exited via ${signal ?? code}`));
        }
      }
      child.once("exit", onExit);
    });
  }

  const hardTimeout = setTimeout(() => child.kill("SIGKILL"), TIMEOUT_MS * 2);
  try {
    const ready = await waitFor(
      (message) => message.type === "ready",
      "ready",
    );
    if (ready.config_marker !== MARKER) {
      throw new Error(
        `host config/data/early-bridge marker mismatch: ${String(ready.config_marker)}`,
      );
    }
    if (
      ready.capabilities?.pure_lua_plugins !== true ||
      ready.capabilities?.remote_files !== true
    ) {
      throw new Error("ready capabilities did not advertise the supported slice");
    }
    const startupPressure = messages.filter(
      (message) =>
        message.type === "action" &&
        String(message.action).startsWith("startup-pressure-"),
    ).length;
    if (startupPressure !== 56) {
      throw new Error(
        `ordinary action reserve admitted ${startupPressure} startup messages; expected 56`,
      );
    }

    for (let index = 0; index < 1100; index += 1) {
      send({ type: "resize", columns: 72, rows: 18 });
    }
    send({ type: "resize", columns: 73, rows: 19 });
    await waitFor(
      (message) =>
        message.type === "screen" &&
        message.columns === 73 &&
        message.rows === 19,
      "post-backpressure resize marker",
    );

    send({
      type: "open-content",
      request_id: "open-smoke",
      path: "src/demo.lua",
      version: "revision-1",
      content: "alpha\n",
      line: 1,
      column: 1,
    });
    const openResult = await waitFor(
      (message) =>
        message.request_id === "open-smoke" &&
        ((message.type === "open-result" && message.ok === true) ||
          message.type === "error"),
      "open result",
    );
    if (openResult.type === "error") {
      throw new Error(
        `remote open failed: ${JSON.stringify(openResult)}`,
      );
    }
    const openPressure = messages.filter(
      (message) =>
        message.type === "action" &&
        String(message.action).startsWith("open-pressure-"),
    ).length;
    if (openPressure !== 56) {
      throw new Error(
        `ordinary action reserve admitted ${openPressure} open messages; expected 56`,
      );
    }
    const pressureSave = messages.find(
      (message) =>
        message.type === "save-request" &&
        message.path === "src/demo.lua" &&
        message.base_version === "revision-1",
    );
    const pressureErrors = messages.filter(
      (message) =>
        message.type === "error" &&
        message.code === "save-pending",
    );
    if (!pressureSave || pressureErrors.length !== 11) {
      throw new Error(
        `critical retry pressure produced ${pressureErrors.length} save-pending errors and save=${Boolean(pressureSave)}`,
      );
    }
    const pressureSaveIndex = messages.indexOf(pressureSave);
    const openResultIndex = messages.indexOf(openResult);
    const pressureErrorIndices = pressureErrors.map((message) =>
      messages.indexOf(message),
    );
    if (
      pressureSaveIndex === -1 ||
      openResultIndex === -1 ||
      pressureErrorIndices.some(
        (index, position) =>
          index <= (position === 0
            ? pressureSaveIndex
            : pressureErrorIndices[position - 1]) ||
          index >= openResultIndex,
      )
    ) {
      throw new Error("critical outbox retry did not preserve FIFO order");
    }
    send({
      type: "save-result",
      request_id: pressureSave.request_id,
      ok: true,
      version: "revision-1",
    });
    await waitFor(
      (message) =>
        message.type === "status" &&
        message.code === "save-complete" &&
        message.request_id === pressureSave.request_id,
      "pressure save completion",
    );
    await waitFor(
      (message) =>
        message.type === "screen" &&
        message.buffer?.path === "src/demo.lua" &&
        message.buffer?.version === "revision-1",
      "remote buffer screen",
    );
    send({
      type: "text",
      text: ":lua vim.brood.send({type='action',action='read-hooks',read_post=vim.b.brood_read_post==true,filetype_event=vim.b.brood_filetype_event,filetype=vim.bo.filetype})",
    });
    send({ type: "key", key: "<CR>" });
    const readHooks = await waitFor(
      (message) =>
        message.type === "action" && message.action === "read-hooks",
      "BufReadPost and FileType lifecycle hooks",
    );
    if (
      readHooks.read_post !== true ||
      readHooks.filetype_event !== "lua" ||
      readHooks.filetype !== "lua"
    ) {
      throw new Error(
        `remote read lifecycle hooks were incomplete: ${JSON.stringify(readHooks)}`,
      );
    }

    send({
      type: "text",
      text: ":lua local source='local value = 42\\nprint(value)'; local tree=vim.treesitter.get_string_parser(source,'lua'):parse()[1]; local query=assert(vim.treesitter.query.get('lua','highlights')); local captures=0; for _ in query:iter_captures(tree:root(),source,0,-1) do captures=captures+1 end; vim.brood.send({type='action',action='treesitter-smoke',root=tree:root():type(),captures=captures})",
    });
    send({ type: "key", key: "<CR>" });
    const treeSitter = await waitFor(
      (message) =>
        message.type === "action" &&
        message.action === "treesitter-smoke",
      "built-in Tree-sitter Lua parse and highlight query",
    );
    if (
      treeSitter.root !== "chunk" ||
      !Number.isSafeInteger(treeSitter.captures) ||
      treeSitter.captures <= 0
    ) {
      throw new Error(
        `Tree-sitter smoke returned an invalid result: ${JSON.stringify(treeSitter)}`,
      );
    }

    send({ type: "text", text: "A" });
    await waitFor(
      (message) => message.type === "screen" && message.mode === "insert",
      "insert mode",
    );
    send({ type: "paste", phase: 1, text: " b" });
    send({ type: "paste", phase: 2, text: "et" });
    send({ type: "paste", phase: 3, text: "aX" });
    await waitFor(
      (message) =>
        message.type === "screen" && message.buffer?.modified === true,
      "modified buffer",
    );
    send({ type: "key", key: "<BS>" });
    send({ type: "key", key: "<Esc>" });
    await waitFor(
      (message) => message.type === "screen" && message.mode === "normal",
      "normal mode",
    );
    send({ type: "text", text: ":write" });
    await waitFor(
      (message) =>
        message.type === "screen" &&
        message.cmdline?.text === "write" &&
        message.cursor?.visible === true &&
        message.cursor?.row === message.rows - 1,
      "command line with visible cursor",
    );
    send({ type: "key", key: "<CR>" });
    const save = await waitFor(
      (message) =>
        message.type === "save-request" &&
        message.request_id !== pressureSave.request_id &&
        message.path === "src/demo.lua",
      "save request",
    );
    if (
      save.path !== "src/demo.lua" ||
      save.base_version !== "revision-1" ||
      save.content !== "alpha beta\n"
    ) {
      throw new Error(`unexpected save request: ${JSON.stringify(save)}`);
    }
    send({
      type: "save-result",
      request_id: save.request_id,
      ok: true,
      version: "revision-2",
    });
    await waitFor(
      (message) =>
        message.type === "status" &&
        message.code === "save-complete" &&
        message.request_id === save.request_id,
      "save completion",
    );
    await waitFor(
      (message) =>
        message.type === "screen" &&
        message.buffer?.version === "revision-2" &&
        message.buffer?.modified === false,
      "clean saved screen",
    );
    send({
      type: "text",
      text: ":lua vim.brood.send({type='action',action='write-hooks',write_pre=vim.b.brood_write_pre==true,write_post=vim.b.brood_write_post==true})",
    });
    send({ type: "key", key: "<CR>" });
    const writeHooks = await waitFor(
      (message) =>
        message.type === "action" && message.action === "write-hooks",
      "BufWritePre and BufWritePost lifecycle hooks",
    );
    if (writeHooks.write_pre !== true || writeHooks.write_post !== true) {
      throw new Error(
        `remote write lifecycle hooks were incomplete: ${JSON.stringify(writeHooks)}`,
      );
    }

    send({
      type: "open-content",
      request_id: "open-crlf-smoke",
      path: "src/crlf.txt",
      version: "crlf-revision-1",
      content: "first\r\nsecond\r\n",
      line: 1,
      column: 1,
    });
    await waitFor(
      (message) =>
        message.type === "open-result" &&
        message.request_id === "open-crlf-smoke" &&
        message.ok === true,
      "CRLF open result",
    );
    send({ type: "text", text: ":write" });
    send({ type: "key", key: "<CR>" });
    const crlfSave = await waitFor(
      (message) =>
        message.type === "save-request" &&
        message.path === "src/crlf.txt",
      "CRLF save request",
    );
    if (crlfSave.content !== "first\r\nsecond\r\n") {
      throw new Error(
        `CRLF content was not preserved: ${JSON.stringify(crlfSave)}`,
      );
    }
    send({
      type: "save-result",
      request_id: crlfSave.request_id,
      ok: true,
      version: "crlf-revision-2",
    });
    await waitFor(
      (message) =>
        message.type === "status" &&
        message.code === "save-complete" &&
        message.request_id === crlfSave.request_id,
      "CRLF save completion",
    );

    const stressPath = [
      "src",
      ...Array.from({ length: 5 }, () => '"'.repeat(180)),
      "screen-stress.txt",
    ].join("/");
    const stressLine = '"\\'.repeat(23);
    const stressContent = `${Array.from({ length: 199 }, () => stressLine).join("\n")}\n`;
    const stressStartIndex = messages.length;
    send({ type: "resize", columns: 72, rows: 200 });
    send({
      type: "open-content-start",
      request_id: "open-screen-stress",
      path: stressPath,
      version: "screen-stress-revision-1",
      size: Buffer.byteLength(stressContent),
      line: 1,
      column: 1,
    });
    for (let offset = 0; offset < stressContent.length; offset += 4000) {
      send({
        type: "open-content-chunk",
        request_id: "open-screen-stress",
        offset,
        content: stressContent.slice(offset, offset + 4000),
      });
    }
    send({
      type: "open-content-end",
      request_id: "open-screen-stress",
    });
    await waitFor(
      (message) =>
        message.type === "open-result" &&
        message.request_id === "open-screen-stress" &&
        message.ok === true,
      "escape-heavy screen stress open",
    );
    const stressScreen = await waitFor(
      (message) =>
        message.type === "screen" &&
        message.rows === 200 &&
        message.buffer?.path?.startsWith("src/") &&
        message.buffer?.version === "screen-stress-revision-1" &&
        message.lines?.length === 200,
      "bounded escape-heavy screen",
    );
    if (Buffer.byteLength(JSON.stringify(stressScreen)) > MAX_LINE_BYTES) {
      throw new Error("bounded stress screen exceeded the JSONL limit");
    }
    if (
      messages
        .slice(stressStartIndex)
        .some(
          (message) =>
            message.type === "error" &&
            (message.code === "screen-too-large" ||
              message.code === "screen-encode-failed"),
        )
    ) {
      throw new Error("escape-heavy state dropped its screen publication");
    }

    send({ type: "text", text: ":BroodDiff" });
    send({ type: "key", key: "<CR>" });
    await waitFor(
      (message) =>
        message.type === "action" && message.action === "diff",
      "typed diff action",
    );

    for (let index = 1; index <= 3; index += 1) {
      const requestId = `open-save-limit-${index}`;
      send({
        type: "open-content",
        request_id: requestId,
        path: `src/save-limit-${index}.txt`,
        version: `save-limit-revision-${index}`,
        content: "seed\n",
        line: 1,
        column: 1,
      });
      await waitFor(
        (message) =>
          message.type === "open-result" &&
          message.request_id === requestId &&
          message.ok === true,
        `save-limit buffer ${index}`,
      );
    }
    send({
      type: "text",
      text: ":lua local bridge=require('brood_bridge'); for _,buffer in ipairs(vim.api.nvim_list_bufs()) do if vim.bo[buffer].buftype == 'acwrite' then vim.api.nvim_buf_set_lines(buffer,0,-1,false,{string.rep('x',9000)}); bridge.request_save(buffer) end end",
    });
    send({ type: "key", key: "<CR>" });
    await waitFor(
      () =>
        messages.filter((message) => message.type === "save-request-start")
          .length === 4,
      "four concurrent streamed saves",
    );
    await waitFor(
      (message) =>
        message.type === "error" &&
        message.code === "save-limit",
      "fifth save rejection",
    );
    const streamedSaves = messages.filter(
      (message) => message.type === "save-request-start",
    ).length;
    if (streamedSaves !== 4) {
      throw new Error(
        `save transfer bound emitted ${streamedSaves} starts; expected 4`,
      );
    }

    send({ type: "focus", focused: false });
    await waitFor(
      (message) =>
        message.type === "screen" && message.cursor?.visible === false,
      "unfocused screen",
    );
    send({ type: "shutdown" });
    await waitFor(
      (message) => message.type === "exit" && message.status === 0,
      "clean runner exit record",
    );

    await waitForCleanExit();
    await runTruncatedEofProbe(options);
    process.stdout.write(
      "neovim_wasm_runner_smoke=verified config=host data=host remote_save=verified action=verified\n",
    );
  } finally {
    clearTimeout(hardTimeout);
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGKILL");
    }
    rmSync(temporary, { recursive: true, force: true });
  }
}

main().catch((error) => {
  process.stderr.write(`${error.stack ?? error}\n`);
  process.exitCode = 1;
});
