import { readFile, realpath, mkdir, rename, rm, stat, open } from "node:fs/promises";
import { resolve, dirname, relative, isAbsolute, join } from "node:path";
import { randomUUID } from "node:crypto";

async function checkedPath(workspace, name) {
  if (!name || isAbsolute(name)) throw new Error("Patch paths must be workspace-relative");
  const root = await realpath(workspace);
  const path = resolve(root, name);
  const inside = candidate => { const offset = relative(root, candidate); return offset !== ".." && !offset.startsWith("../") && !isAbsolute(offset); };
  if (!inside(path) || path === root) throw new Error(`Patch path escapes workspace: ${name}`);
  let existing = path;
  while (true) {
    try {
      if (!inside(await realpath(existing))) throw new Error(`Patch symlink escapes workspace: ${name}`);
      break;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
      existing = dirname(existing);
    }
  }
  return path;
}

export async function applyPatch(patch, workspace = process.cwd(), signal) {
  if (typeof patch !== "string" || Buffer.byteLength(patch) > 2 * 1024 * 1024) throw new Error("patch must be text of at most 2 MiB");
  const lines = patch.replaceAll("\r\n", "\n").trimEnd().split("\n");
  if (lines.shift() !== "*** Begin Patch" || lines.pop() !== "*** End Patch") throw new Error("Patch must start with *** Begin Patch and end with *** End Patch");
  const changes = [], paths = new Set();
  while (lines.length) {
    signal?.throwIfAborted();
    const header = /^\*\*\* (Add|Update|Delete) File: (.+)$/.exec(lines.shift());
    if (!header) throw new Error("Expected Add, Update, or Delete File header");
    const [, kind, name] = header;
    const path = await checkedPath(workspace, name);
    let original, mode = 0o644;
    try { original = await readFile(path, "utf8"); mode = (await stat(path)).mode & 0o777; }
    catch (error) { if (error.code !== "ENOENT") throw error; }
    if (kind === "Add" && original !== undefined) throw new Error(`File already exists: ${name}`);
    if (kind !== "Add" && original === undefined) throw new Error(`File does not exist: ${name}`);
    let destination = path;
    if (kind === "Update" && lines[0]?.startsWith("*** Move to: ")) destination = await checkedPath(workspace, lines.shift().slice(13));
    if (destination !== path) {
      try { await stat(destination); throw new Error(`Move destination already exists: ${destination}`); }
      catch (error) { if (error.code !== "ENOENT") throw error; }
    }
    if (paths.has(path) || paths.has(destination)) throw new Error("A patch may change each path only once");
    paths.add(path); paths.add(destination);
    let updated;
    if (kind === "Add") {
      const added = [];
      while (lines[0]?.startsWith("+")) added.push(lines.shift().slice(1));
      updated = added.join("\n") + "\n";
    } else if (kind === "Update") {
      const crlf = original.includes("\r\n");
      const source = original.replaceAll("\r\n", "\n").split("\n");
      if (source.at(-1) === "") source.pop();
      let cursor = 0, hunks = 0;
      while (lines.length && !/^\*\*\* (Add|Update|Delete) File: /.test(lines[0])) {
        let anchor;
        if (lines[0].startsWith("@@")) { anchor = lines.shift().slice(2).trim(); }
        const before = [], after = [];
        let end = false;
        while (lines.length && !lines[0].startsWith("@@") && !lines[0].startsWith("*** ")) {
          const line = lines.shift(), prefix = line[0];
          if (![" ", "+", "-"].includes(prefix)) throw new Error(`Invalid patch line in ${name}`);
          if (prefix !== "+") before.push(line.slice(1));
          if (prefix !== "-") after.push(line.slice(1));
        }
        if (lines[0] === "*** End of File") { lines.shift(); end = true; }
        if (!before.length && !after.length) throw new Error(`Empty patch hunk in ${name}`);
        if (anchor) {
          const index = source.indexOf(anchor, cursor);
          if (index < 0) throw new Error(`Patch anchor not found in ${name}: ${anchor}`);
          cursor = index + 1;
        }
        const matches = [];
        if (!before.length) matches.push(source.length);
        else for (let index = cursor; index <= source.length - before.length; index++) {
          if ((!end || index + before.length === source.length) && before.every((line, offset) => source[index + offset] === line)) matches.push(index);
        }
        if (matches.length !== 1) throw new Error(`Patch context ${matches.length ? "is ambiguous" : "not found"} in ${name}; include more exact context`);
        source.splice(matches[0], before.length, ...after);
        cursor = matches[0] + after.length;
        hunks++;
      }
      if (!hunks) throw new Error(`No update hunks in ${name}`);
      updated = source.join(crlf ? "\r\n" : "\n") + (crlf ? "\r\n" : "\n");
    }
    changes.push({ path, destination, name, kind, original, updated, mode });
  }
  if (!changes.length) throw new Error("Patch contains no file changes");
  // Validate every file before touching the workspace. Writes use adjacent
  // temporary files, and a failed commit restores already changed files.
  const committed = [];
  const replace = async (path, content, mode) => {
    await mkdir(dirname(path), { recursive: true });
    const temp = join(dirname(path), `.brood-patch-${randomUUID()}`);
    try {
      const file = await open(temp, "wx", mode);
      try { await file.writeFile(content); } finally { await file.close(); }
      await rename(temp, path);
    } finally { await rm(temp, { force: true }); }
  };
  try {
    for (const change of changes) {
      signal?.throwIfAborted();
      const current = await readFile(change.path, "utf8").catch(error => { if (error.code === "ENOENT") return undefined; throw error; });
      if (current !== change.original) throw new Error(`File changed while preparing patch: ${change.name}`);
      committed.push(change);
      if (change.kind === "Delete") await rm(change.path);
      else {
        await replace(change.destination, change.updated, change.mode);
        if (change.destination !== change.path) await rm(change.path);
      }
    }
  } catch (error) {
    for (const change of committed.reverse()) {
      if (change.original === undefined) await rm(change.destination, { force: true });
      else {
        await replace(change.path, change.original, change.mode);
        if (change.destination !== change.path) await rm(change.destination, { force: true });
      }
    }
    throw error;
  }
  return changes.map(change => `${change.kind}: ${change.name}`).join("\n");
}
