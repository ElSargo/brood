#!/usr/bin/env node

import {
  lstatSync,
  readFileSync,
  readdirSync,
  statSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve, sep } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const VERSION = 1;
const MAX_LINE_BYTES = 12 * 1024;
const MAX_INLINE_BYTES = 8 * 1024;
const MAX_FILE_BYTES = 8 * 1024 * 1024;
// Keep MEMFS metadata bounded. When this high-water mark is reached, the
// parser retains the next complete record and pauses stdin until Neovim drains
// the inbox, so accepted file and paste streams are never truncated.
const MAX_PENDING_MESSAGES = 1024;
const MAX_MIRROR_FILES = 50_000;
const MAX_MIRROR_BYTES = 256 * 1024 * 1024;
const DEFAULT_COLUMNS = 100;
const DEFAULT_ROWS = 32;
const STATIC_TREE_SITTER_LANGUAGES = [
  "c",
  "diff",
  "lua",
  "markdown",
  "markdown_inline",
  "query",
  "vim",
  "vimdoc",
];

const runtimeDirectory = dirname(fileURLToPath(import.meta.url));
const bridgeSource = readFileSync(join(runtimeDirectory, "brood.lua"), "utf8");
const textEncoder = new TextEncoder();

function usage() {
  return [
    "usage: runner.mjs [--module-dir DIR] [--config-dir DIR] [--data-dir DIR]",
    "                  [--columns N] [--rows N]",
  ].join("\n");
}

function parseInteger(name, value, minimum, maximum) {
  if (!/^[0-9]+$/.test(value ?? "")) {
    throw new Error(`${name} must be an integer`);
  }
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < minimum || parsed > maximum) {
    throw new Error(`${name} must be between ${minimum} and ${maximum}`);
  }
  return parsed;
}

function parseArguments(argv) {
  const xdgConfig = process.env.XDG_CONFIG_HOME
    ? resolve(process.env.XDG_CONFIG_HOME)
    : join(homedir(), ".config");
  const xdgData = process.env.XDG_DATA_HOME
    ? resolve(process.env.XDG_DATA_HOME)
    : join(homedir(), ".local", "share");
  const options = {
    moduleDirectory: runtimeDirectory,
    configDirectory: join(xdgConfig, "nvim"),
    dataDirectory: join(xdgData, "nvim"),
    columns: DEFAULT_COLUMNS,
    rows: DEFAULT_ROWS,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    const value = argv[index + 1];
    switch (argument) {
      case "--module-dir":
        if (value === undefined) throw new Error("--module-dir needs a path");
        options.moduleDirectory = resolve(value);
        index += 1;
        break;
      case "--config-dir":
        if (value === undefined) throw new Error("--config-dir needs a path");
        options.configDirectory = resolve(value);
        index += 1;
        break;
      case "--data-dir":
        if (value === undefined) throw new Error("--data-dir needs a path");
        options.dataDirectory = resolve(value);
        index += 1;
        break;
      case "--columns":
        options.columns = parseInteger("--columns", value, 20, 400);
        index += 1;
        break;
      case "--rows":
        options.rows = parseInteger("--rows", value, 4, 200);
        index += 1;
        break;
      case "--help":
      case "-h":
        process.stdout.write(`${usage()}\n`);
        process.exit(0);
        break;
      default:
        throw new Error(`unknown argument: ${argument}`);
    }
  }
  return options;
}

function emit(value) {
  const encoded = JSON.stringify(value);
  const bytes = textEncoder.encode(encoded).byteLength;
  if (bytes > MAX_LINE_BYTES) {
    const fallback = JSON.stringify({
      v: VERSION,
      type: "error",
      code: "output-too-large",
      message: `runner output was ${bytes} bytes; limit is ${MAX_LINE_BYTES}`,
    });
    process.stdout.write(`${fallback}\n`);
    return false;
  }
  process.stdout.write(`${encoded}\n`);
  return true;
}

function diagnostic(message) {
  process.stderr.write(`brood-neovim-wasm: ${message}\n`);
}

function isMissing(path) {
  try {
    lstatSync(path);
    return false;
  } catch (error) {
    if (error?.code === "ENOENT") return true;
    throw error;
  }
}

const skippedDirectories = new Set([
  ".git",
  ".hg",
  ".svn",
  "node_modules",
  "target",
]);
const skippedNativeSuffixes = [
  ".dll",
  ".dylib",
  ".node",
  ".o",
  ".so",
  ".wasm",
];

function shouldSkipFile(name) {
  const lower = name.toLowerCase();
  return skippedNativeSuffixes.some((suffix) => lower.endsWith(suffix));
}

function collectMirror(root, label) {
  const files = [];
  const directories = new Set([""]);
  const skipped = [];
  let totalBytes = 0;

  if (isMissing(root)) {
    return { label, root, files, directories: [""], skipped, totalBytes };
  }

  const rootStat = statSync(root);
  if (!rootStat.isDirectory()) {
    throw new Error(`${label} path is not a directory: ${root}`);
  }

  function walk(hostPath, relativePath, ancestors) {
    const linkStat = lstatSync(hostPath);
    const followsLink = linkStat.isSymbolicLink();
    const stat = followsLink ? statSync(hostPath) : linkStat;

    if (stat.isDirectory()) {
      const identity = `${stat.dev}:${stat.ino}`;
      if (ancestors.has(identity)) {
        skipped.push(`${relativePath || "."}: symlink cycle`);
        return;
      }
      const nextAncestors = new Set(ancestors);
      nextAncestors.add(identity);
      directories.add(relativePath);
      const names = readdirSync(hostPath).sort();
      for (const name of names) {
        if (skippedDirectories.has(name)) {
          skipped.push(`${join(relativePath, name)}: excluded directory`);
          continue;
        }
        walk(join(hostPath, name), join(relativePath, name), nextAncestors);
      }
      return;
    }

    if (!stat.isFile()) {
      skipped.push(`${relativePath}: not a regular file`);
      return;
    }
    if (shouldSkipFile(relativePath)) {
      skipped.push(`${relativePath}: native module is not wasm-compatible`);
      return;
    }
    if (stat.size > MAX_FILE_BYTES) {
      throw new Error(
        `${label} file exceeds ${MAX_FILE_BYTES} bytes: ${relativePath}`,
      );
    }
    if (files.length >= MAX_MIRROR_FILES) {
      throw new Error(`${label} contains more than ${MAX_MIRROR_FILES} files`);
    }
    totalBytes += stat.size;
    if (totalBytes > MAX_MIRROR_BYTES) {
      throw new Error(`${label} exceeds the ${MAX_MIRROR_BYTES}-byte mirror limit`);
    }
    files.push({
      relativePath,
      contents: readFileSync(hostPath),
    });
    directories.add(dirname(relativePath) === "." ? "" : dirname(relativePath));
  }

  walk(root, "", new Set());
  return {
    label,
    root,
    files,
    directories: [...directories].sort(
      (left, right) =>
        left.split(sep).length - right.split(sep).length || left.localeCompare(right),
    ),
    skipped,
    totalBytes,
  };
}

function ensureMemfsDirectory(FS, path, mode = 0o755) {
  let current = "";
  for (const part of path.split("/").filter(Boolean)) {
    current += `/${part}`;
    try {
      FS.mkdir(current, mode);
    } catch (error) {
      if (error?.code !== "EEXIST" && error?.errno !== 20) {
        try {
          FS.stat(current);
        } catch {
          throw error;
        }
      }
    }
  }
}

function copyMirrorToMemfs(FS, mirror, destination) {
  ensureMemfsDirectory(FS, destination);
  for (const directory of mirror.directories) {
    if (directory !== "") {
      ensureMemfsDirectory(FS, join(destination, directory));
    }
  }
  for (const file of mirror.files) {
    const target = join(destination, file.relativePath);
    ensureMemfsDirectory(FS, dirname(target));
    FS.writeFile(target, file.contents, { mode: 0o444 });
    FS.chmod(target, 0o444);
  }
  for (const directory of [...mirror.directories].reverse()) {
    FS.chmod(directory === "" ? destination : join(destination, directory), 0o555);
  }
}

function installStaticParserMarkers(FS) {
  // Neovim's WASM target links these parsers into its static registry, but its
  // install step does not create the parser filenames that runtime Lua uses
  // for discovery. Empty read-only markers make discovery reach the static
  // loader; they are never loaded as native objects.
  const directory = "/brood/parser";
  ensureMemfsDirectory(FS, directory);
  for (const language of STATIC_TREE_SITTER_LANGUAGES) {
    const path = `${directory}/${language}.so`;
    FS.writeFile(path, new Uint8Array(), { mode: 0o444 });
    FS.chmod(path, 0o444);
  }
  FS.chmod(directory, 0o555);
}

function makeArgv(module, arguments_) {
  const pointers = arguments_.map((argument) => {
    const length = module.lengthBytesUTF8(argument) + 1;
    const pointer = module._malloc(length);
    module.stringToUTF8(argument, pointer, length);
    return pointer;
  });
  const argv = module._malloc((pointers.length + 1) * 4);
  for (let index = 0; index < pointers.length; index += 1) {
    module.setValue(argv + index * 4, pointers[index], "*");
  }
  module.setValue(argv + pointers.length * 4, 0, "*");
  return { argc: pointers.length, argv };
}

function safeOutboxMessage(raw, filename) {
  const bytes = textEncoder.encode(raw).byteLength;
  if (bytes > MAX_LINE_BYTES) {
    return {
      v: VERSION,
      type: "error",
      code: "guest-output-too-large",
      message: `${filename} was ${bytes} bytes; limit is ${MAX_LINE_BYTES}`,
    };
  }
  try {
    const parsed = JSON.parse(raw);
    if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
      throw new Error("message must be a JSON object");
    }
    return parsed;
  } catch (error) {
    return {
      v: VERSION,
      type: "error",
      code: "invalid-guest-output",
      message: `${filename}: ${error.message}`,
    };
  }
}

function listQueue(FS, directory) {
  try {
    return FS.readdir(directory)
      .filter((name) => name !== "." && name !== "..")
      .sort();
  } catch {
    return [];
  }
}

function validateInput(message) {
  if (message === null || typeof message !== "object" || Array.isArray(message)) {
    throw new Error("input must be a JSON object");
  }
  if (message.v !== VERSION) {
    throw new Error(`unsupported protocol version: ${String(message.v)}`);
  }
  if (typeof message.type !== "string" || message.type.length === 0) {
    throw new Error("input needs a non-empty type");
  }
  if (
    (message.type === "open-content" ||
      message.type === "input" ||
      message.type === "paste" ||
      message.type === "text") &&
    typeof (message.content ?? message.text) === "string"
  ) {
    const bytes = textEncoder.encode(message.content ?? message.text).byteLength;
    if (message.type === "open-content" && bytes > MAX_INLINE_BYTES) {
      throw new Error(
        `inline open content exceeds ${MAX_INLINE_BYTES} bytes; use chunked input`,
      );
    }
    if (bytes > MAX_FILE_BYTES) {
      throw new Error(`input text exceeds ${MAX_FILE_BYTES} bytes`);
    }
  }
  if (
    message.type === "paste" &&
    ![-1, 1, 2, 3].includes(message.phase)
  ) {
    throw new Error("paste phase must be -1, 1, 2, or 3");
  }
  if (
    message.type === "open-content-start" &&
    (!Number.isSafeInteger(message.size) ||
      message.size < 0 ||
      message.size > MAX_FILE_BYTES)
  ) {
    throw new Error(`open content size must be between 0 and ${MAX_FILE_BYTES}`);
  }
}

function createInputParser(onMessage, canAccept) {
  let pending = Buffer.alloc(0);
  let discardingOversizedLine = false;

  function drain() {
    while (true) {
      const newline = pending.indexOf(0x0a);
      if (newline === -1) {
        if (pending.byteLength > MAX_LINE_BYTES) {
          pending = Buffer.alloc(0);
          discardingOversizedLine = true;
          emit({
            v: VERSION,
            type: "error",
            code: "input-too-large",
            message: `input line exceeds ${MAX_LINE_BYTES} bytes`,
          });
        }
        return true;
      }

      const line = pending.subarray(0, newline);
      if (discardingOversizedLine) {
        pending = pending.subarray(newline + 1);
        discardingOversizedLine = false;
        continue;
      }
      if (line.byteLength === 0) {
        pending = pending.subarray(newline + 1);
        continue;
      }
      if (line.byteLength > MAX_LINE_BYTES) {
        pending = pending.subarray(newline + 1);
        emit({
          v: VERSION,
          type: "error",
          code: "input-too-large",
          message: `input line was ${line.byteLength} bytes; limit is ${MAX_LINE_BYTES}`,
        });
        continue;
      }
      let message;
      try {
        message = JSON.parse(line.toString("utf8"));
        validateInput(message);
      } catch (error) {
        pending = pending.subarray(newline + 1);
        emit({
          v: VERSION,
          type: "error",
          code: "invalid-input",
          message: error.message,
        });
        continue;
      }
      if (!canAccept()) {
        return false;
      }
      try {
        onMessage(message);
        pending = pending.subarray(newline + 1);
      } catch (error) {
        emit({
          v: VERSION,
          type: "error",
          code: "input-enqueue-failed",
          message: error.message,
        });
        return false;
      }
    }
  }

  return {
    push(chunk) {
      pending = Buffer.concat([pending, chunk]);
      return drain();
    },
    drain,
    finish() {
      if (!drain()) {
        return false;
      }
      if (pending.byteLength > 0) {
        emit({
          v: VERSION,
          type: "error",
          code: "invalid-input",
          message: "unterminated JSONL record at end of input",
        });
        pending = Buffer.alloc(0);
      }
      discardingOversizedLine = false;
      return true;
    },
    hasPending() {
      return pending.byteLength > 0;
    },
  };
}

async function loadCreateNvim(modulePath) {
  const require = createRequire(import.meta.url);
  const loaded = require(modulePath);
  const factory = loaded?.default ?? loaded;
  if (typeof factory !== "function") {
    throw new Error(`${modulePath} did not export the Emscripten createNvim factory`);
  }
  return factory;
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  const modulePath = join(options.moduleDirectory, "nvim.js");
  const configMirror = collectMirror(options.configDirectory, "Neovim config");
  const dataMirror = collectMirror(options.dataDirectory, "Neovim data");
  const mirrorFiles = configMirror.files.length + dataMirror.files.length;
  const mirrorBytes = configMirror.totalBytes + dataMirror.totalBytes;
  if (mirrorFiles > MAX_MIRROR_FILES) {
    throw new Error(
      `Neovim config and data contain more than ${MAX_MIRROR_FILES} files combined`,
    );
  }
  if (mirrorBytes > MAX_MIRROR_BYTES) {
    throw new Error(
      `Neovim config and data exceed the ${MAX_MIRROR_BYTES}-byte combined mirror limit`,
    );
  }
  const createNvim = await loadCreateNvim(modulePath);
  let module;
  let nextInboxSequence = 1;
  let pendingInboxMessages = 0;
  let lastScreenRevision = -1;
  let shuttingDown = false;
  let stdinPaused = false;
  let shutdownPending = false;
  let forcingCleanExit = false;

  const mirrorSummary = {
    config: {
      files: configMirror.files.length,
      bytes: configMirror.totalBytes,
      skipped: configMirror.skipped.length,
    },
    data: {
      files: dataMirror.files.length,
      bytes: dataMirror.totalBytes,
      skipped: dataMirror.skipped.length,
    },
  };

  module = await createNvim({
    locateFile(path) {
      return join(options.moduleDirectory, path);
    },
    noInitialRun: true,
    print(text) {
      diagnostic(`nvim stdout: ${text}`);
    },
    printErr(text) {
      diagnostic(`nvim stderr: ${text}`);
    },
    preRun: [
      (instance) => {
        instance.ENV.HOME = "/home/brood";
        instance.ENV.XDG_CONFIG_HOME = "/home/brood/.config";
        instance.ENV.XDG_DATA_HOME = "/home/brood/.local/share";
        instance.ENV.XDG_STATE_HOME = "/home/brood/.local/state";
        instance.ENV.XDG_CACHE_HOME = "/home/brood/.cache";
        instance.ENV.VIMRUNTIME = "/runtime";
        instance.ENV.TERM = "dumb";

        ensureMemfsDirectory(instance.FS, "/home/brood/.config");
        ensureMemfsDirectory(instance.FS, "/home/brood/.local/share");
        ensureMemfsDirectory(instance.FS, "/home/brood/.local/state");
        ensureMemfsDirectory(instance.FS, "/home/brood/.cache");
        ensureMemfsDirectory(instance.FS, "/workspace");
        ensureMemfsDirectory(instance.FS, "/brood/lua");
        ensureMemfsDirectory(instance.FS, "/brood/io/inbox");
        ensureMemfsDirectory(instance.FS, "/brood/io/outbox");
        installStaticParserMarkers(instance.FS);

        copyMirrorToMemfs(
          instance.FS,
          configMirror,
          "/home/brood/.config/nvim",
        );
        copyMirrorToMemfs(
          instance.FS,
          dataMirror,
          "/home/brood/.local/share/nvim",
        );
        instance.FS.writeFile("/brood/lua/brood_bridge.lua", bridgeSource, {
          mode: 0o444,
        });
        instance.FS.writeFile(
          "/brood/io/mirror.json",
          JSON.stringify(mirrorSummary),
        );
        instance.FS.writeFile(
          "/brood/io/dimensions.json",
          JSON.stringify({ columns: options.columns, rows: options.rows }),
        );
      },
    ],
  });

  function enqueue(message) {
    if (pendingInboxMessages >= MAX_PENDING_MESSAGES) {
      throw new Error("attempted to enqueue past the runner inbox high-water mark");
    }
    const filename = `${String(nextInboxSequence).padStart(12, "0")}.json`;
    nextInboxSequence += 1;
    module.FS.writeFile(
      `/brood/io/inbox/${filename}`,
      `${JSON.stringify(message)}\n`,
    );
    pendingInboxMessages += 1;
  }

  const inputParser = createInputParser(
    (message) => {
      if (shuttingDown) {
        throw new Error("input received after shutdown");
      }
      enqueue(message);
      if (message.type === "shutdown") {
        shuttingDown = true;
      }
    },
    () => pendingInboxMessages < MAX_PENDING_MESSAGES,
  );

  function pauseStdin() {
    if (!stdinPaused) {
      process.stdin.pause();
      stdinPaused = true;
    }
  }

  function enqueuePendingShutdown() {
    if (
      shutdownPending &&
      !inputParser.hasPending() &&
      pendingInboxMessages < MAX_PENDING_MESSAGES
    ) {
      enqueue({ v: VERSION, type: "shutdown" });
      shutdownPending = false;
    }
  }

  function finishCleanExit() {
    if (forcingCleanExit) return;
    forcingCleanExit = true;
    clearInterval(poll);
    const encoded = `${JSON.stringify({
      v: VERSION,
      type: "exit",
      status: 0,
    })}\n`;
    // The upstream module intentionally uses EXIT_RUNTIME=0 for embedding.
    // Once Neovim has consumed our shutdown message and synchronously written
    // the marker, its main loop halts but Emscripten keeps the Node runtime
    // alive. Flush the final protocol record, then end this dedicated runner.
    process.stdout.end(encoded, () => process.exit(0));
  }

  process.stdin.on("data", (chunk) => {
    if (!inputParser.push(chunk)) {
      pauseStdin();
    }
  });
  process.stdin.on("end", () => {
    if (!shuttingDown) shutdownPending = true;
    shuttingDown = true;
    inputParser.finish();
    enqueuePendingShutdown();
  });
  process.stdin.resume();

  const poll = setInterval(() => {
    pendingInboxMessages = listQueue(module.FS, "/brood/io/inbox").length;
    if (pendingInboxMessages < MAX_PENDING_MESSAGES) {
      const inputDrained = shuttingDown
        ? inputParser.finish()
        : inputParser.drain();
      if (!inputDrained) {
        pauseStdin();
      } else if (stdinPaused && !shuttingDown) {
        stdinPaused = false;
        process.stdin.resume();
      }
      enqueuePendingShutdown();
    }
    for (const filename of listQueue(module.FS, "/brood/io/outbox")) {
      const path = `/brood/io/outbox/${filename}`;
      try {
        const raw = module.FS.readFile(path, { encoding: "utf8" });
        module.FS.unlink(path);
        emit(safeOutboxMessage(raw, filename));
      } catch (error) {
        emit({
          v: VERSION,
          type: "error",
          code: "outbox-read-failed",
          message: `${filename}: ${error.message}`,
        });
      }
    }

    try {
      const raw = module.FS.readFile("/brood/io/screen.json", {
        encoding: "utf8",
      });
      const screen = safeOutboxMessage(raw, "screen.json");
      if (
        screen.type !== "error" &&
        Number.isSafeInteger(screen.revision) &&
        screen.revision > lastScreenRevision
      ) {
        lastScreenRevision = screen.revision;
        emit(screen);
      }
    } catch {
      // The bridge creates the coalesced screen slot after startup.
    }

    if (
      shuttingDown &&
      listQueue(module.FS, "/brood/io").includes("shutdown-ready")
    ) {
      module.FS.unlink("/brood/io/shutdown-ready");
      finishCleanExit();
    }
  }, 8);

  const { argc, argv } = makeArgv(module, [
    "nvim",
    "--headless",
    "--cmd",
    "set runtimepath^=/brood",
    "--cmd",
    "lua require('brood_bridge').install()",
    "-c",
    "lua require('brood_bridge').start()",
  ]);

  const result = module._nvim_main(argc, argv);
  if (result && typeof result.then === "function") {
    result
      .then((status) => {
        clearInterval(poll);
        emit({ v: VERSION, type: "exit", status: Number(status) || 0 });
        process.exitCode = Number(status) || 0;
      })
      .catch((error) => {
        clearInterval(poll);
        diagnostic(error.stack ?? String(error));
        emit({
          v: VERSION,
          type: "error",
          code: "nvim-failed",
          message: error.message ?? String(error),
        });
        process.exitCode = 1;
      });
  }
}

main().catch((error) => {
  diagnostic(error.stack ?? String(error));
  emit({
    v: VERSION,
    type: "error",
    code: "runner-start-failed",
    message: error.message ?? String(error),
  });
  process.exitCode = 1;
});
