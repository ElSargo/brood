# Client-local Neovim actor

`neovim-client` is the presentation half of the remote editor. It owns the
pane, input, and rendering on the client. A separate session actor named
`neovim-files` owns workspace file access on the selected backend. File bytes
cross the actor boundary; Neovim itself never runs on the backend.

At start the actor launches one long-lived, allowlisted `node` subprocess with
`share/brood/neovim-wasm/runner.mjs`, relative to the installed client actor
package root. The runner loads the browser/WASM Neovim runtime and the user's
local `~/.config/nvim`, including pure Lua plugins available there. The
read-only config/data snapshot omits native modules and large generated
directories, is bounded to 50,000 files and 256 MiB, and is not synchronized
back to the host. Subprocess pipes are consumed by a worker. Pane dispatch only
updates bounded guest state and signals notifications, so a slow runtime, file
transfer, or pipe cannot block the actor mailbox.
The Wasm core's statically linked `c`, `diff`, `lua`, `markdown`,
`markdown_inline`, `query`, `vim`, and `vimdoc` Tree-sitter parsers remain
available; additional native parser objects from the host snapshot do not.

The presentation lifecycle supplies the exact backend and surface-session IDs.
Every file request uses those IDs in `SendSessionMessage`; replies are accepted
only from the authenticated sender
`session:<backend-id>:neovim-files`. The actor sends `editor-client-ready` only
after Node/Wasm Neovim has reported ready. Session init then binds that exact
presentation identity to `neovim-files`; the client accepts the matching
`editor-client-authorized` reply and returns `editor-files-ready` before init
flushes queued opens. Clean stop and runner failure unregister the identity so
init cannot retain an unusable editor address.

## Editor control

`brood.client-init` opens a file with:

```json
{"kind":"editor-open","path":"workspace/relative","line":12,"column":4}
```

`line` and `column` are optional and one-based. Paths must be normalized,
workspace-relative UTF-8 paths. The client requests the file in bounded chunks,
then supplies its content and backend revision to the local runner. Saves carry
the revision back to `neovim-files`, which performs the authoritative
conflict-checked write. The same envelope is accepted from the authenticated
`session:<backend-id>:init` sender so the session root can flush opens queued
before `editor-client-ready`; no other session sender is trusted for opens.
The client starts at most four file transfers at once and retains another 32
opens in arrival order, draining one whenever a transfer or runner open
completes.

The backend file protocol is JSON v1. It accepts UTF-8 text without NUL bytes;
consistent CRLF line endings are retained through Neovim's `dos` file format.
Opening a remote buffer emits `BufReadPre` and `BufReadPost`, allowing the
user's normal filetype detection and `FileType` hooks to run. A remote write
emits `BufWritePre` before the replacement is captured and `BufWritePost` only
after backend success.

- Supported client requests: `hello`, `open`, `read`, `open-abort`,
  `save-begin`, `save-chunk`, `save-commit`, and `save-abort`.
- Backend replies: `hello`, `open`, `read`, `save-ready`,
  `save-chunk-accepted`, `save-complete`, and `error`.
- File chunks use lowercase `data_hex`; each decoded chunk is at most 7168
  bytes. A file is at most 8 MiB and at most four transfers may be live.
- An open/read reply carries `request`, `path`, `revision`, `size`, `offset`,
  `data_hex`, and `eof`. A save begins with `transfer`, `path`,
  `base_revision`, and `size`; every accepted chunk advances the byte offset
  before the commit is sent. `save-chunk-accepted.next_offset` must exactly
  equal the end of the in-flight chunk.
- A save is successful only when `save-complete` exactly matches its request,
  transfer, path, size, and SHA-256 hash of the bytes sent.
- A client-side validation failure sends the corresponding abort before the
  local slot is reused. Four file-sized transfers fit the 32 MiB aggregate
  client file-state bound.

## Runner JSONL

Each pipe record is one JSON object terminated by `\n`, with `v: 1`. Actor to
runner records are:

- `key { key }`, where `key` uses Neovim key notation;
- `text { text }` for ordinary keyboard text through `nvim_input`;
- `paste { text, phase }` for a retained, streamed `nvim_paste` transaction;
- `resize { columns, rows }`;
- `focus { focused }`;
- `open-content { request_id, path, version, content, line?, column? }`;
- `open-content-start`, ordered `open-content-chunk`, and
  `open-content-end` for larger UTF-8 files;
- `save-result { request_id, ok, version?, error? }`; and
- `shutdown`.

Runner to actor records are:

- `ready`, `status`, and `error`;
- `screen { revision, columns, rows, lines, cursor, mode, buffer, cmdline,
  message }`, where `cmdline` is null or `{ type, text, cursor }`;
- `open-result { request_id, ok, path, version? }`;
- `save-request { request_id, path, base_version, content }`, or the ordered
  `save-request-start` / `save-request-chunk` / `save-request-end` form;
- `action { action, ... }` for `diff`, `terminal`, `agent`, or `generic`;
- `actor-message { target, body }` for the generic Lua messaging bridge.

Chunk offsets and sizes are UTF-8 byte counts, not JavaScript UTF-16 indices.
Runner dimensions are clamped to 20–400 columns and 4–200 rows.
The client accepts compatible aliases for screen columns/cursor fields, but the
names above are the canonical protocol.
The Node bridge pauses its input pipe at the bounded MEMFS inbox high-water
mark and retains the next complete record until space is available. The Lua
bridge reserves outbox capacity and bounded retries for ready, open, save, and
protocol-result records; ordinary user actions cannot crowd those records out.
At most four Lua saves may await results at once.

Runner actions become a typed `editor-action` envelope sent locally to
`brood.client-init`. This keeps diff panes, terminal creation, coding-agent
prompts, and future generic actions in the ordinary client messaging system.

Screen snapshots render through the public Brood canvas packet and native
monospace text APIs. Runner queues, screen state, file content, and transfer
assemblies have explicit bounds; no file assembly may exceed 8 MiB.
Encoded screen records are capped at 12 KiB and adaptively trim low-priority
text under escape-heavy state without dropping the viewport or its cursor.

The headless screen fallback reconstructs the active buffer's logical lines
with one text style, plus cursor, command line, and latest message. It does not
yet reproduce syntax highlighting, line numbers, signs, folds, wrapping,
conceal, extmarks, virtual text, popup menus, floating windows, or split-window
composition. Lua plugins still load and can use the messaging API, but plugins
whose UI depends on those surfaces do not yet have a complete visual frontend.
