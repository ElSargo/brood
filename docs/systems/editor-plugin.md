---
status: target
type: system
authority: product
last-reviewed: 2026-07-29
code-links:
  - plugins/neovim-client/src/lib.rs
  - plugins/neovim-files/src/lib.rs
  - plugins/neovim-runtime/runner.mjs
  - plugins/neovim-runtime/brood.lua
  - examples/wasm-init-actor/src/lib.rs
  - plugins/session-switcher/src/lib.rs
---

# First-Party Neovim Plugin

The Brood editor is Neovim compiled to WebAssembly. Neovim runs on the
presentation client; it is never deployed to, or executed by, a remote
backend. Remote sessions expose a file actor instead. This keeps editor state
and latency local while file authority stays with the backend that owns the
workspace.

## Two-Actor Boundary

Every open editor consists of two actors:

- `neovim-client` is a presentation actor. It starts the packaged
  Emscripten/Node Neovim runtime, owns buffers, modes, undo state, the logical
  viewport, pane rendering, and input.
- `neovim-files` is a session actor. It owns workspace-relative reads and
  atomic, conflict-checked writes on the backend.

The presentation actor is bound to an explicit backend and session. It sends
file messages directly to that realm's `neovim-files` actor and accepts file
replies only from that exact address. A remote workspace path is an identity,
not a client-side process working directory: the local runtime starts from its
installed package directory.

The session init actor starts `neovim-files` with the workspace write grant.
The client init actor starts `neovim-client` with only the packaged runtime and
the allowlisted `node` subprocess. Neither actor receives the other side's
filesystem.

File authority is delegated explicitly. After the local Wasm runtime is ready,
the client identifies itself to session init over the host-authenticated
presentation lane. Init binds that exact identity to `neovim-files` over its
host-authenticated same-realm lane. The file actor acknowledges the client
directly, and only then does the client report `editor-files-ready` to init.
Queued opens are not forwarded before that round trip, so correctness does not
depend on message ordering across actors. Client stop or replacement revokes
the binding and its bounded protocol state.

## Host Neovim Configuration

At startup the local runner mirrors the client's Neovim config and data
directories into Neovim's in-memory filesystem. The defaults are
`$XDG_CONFIG_HOME/nvim` and `$XDG_DATA_HOME/nvim`, with the usual `$HOME`
fallbacks. It then loads the Brood bridge before entering the user's normal
configuration.

Pure Lua configuration and plugins in that snapshot therefore run inside the
local Wasm Neovim. The snapshot is read-only and is rebuilt on editor start;
plugin state is not synchronized back to the host. Native module suffixes and
large generated directories such as `.git`, `node_modules`, and `target` are
excluded. Startup rejects a mirror with more than 50,000 files, 256 MiB total,
or an individual file over 8 MiB. A plugin that requires a native Lua module,
an arbitrary host executable, writable state in the mirrored tree, or direct
host filesystem access needs a Brood integration rather than an implicit
authority grant.

The packaged core statically includes Neovim's `c`, `diff`, `lua`, `markdown`,
`markdown_inline`, `query`, `vim`, and `vimdoc` Tree-sitter parsers. Those
languages support normal parser and query APIs inside Wasm. Additional parser
shared objects from the host data snapshot are native modules and are not
loaded.

Remote opens execute Neovim's ordinary `BufReadPre` and `BufReadPost`
lifecycle, including configured filetype detection and modeline processing.
Remote writes execute `BufWritePre` before content is captured and
`BufWritePost` only after the backend confirms the atomic replacement. This
keeps pure-Lua lazy loading and synchronous format-on-save hooks on the normal
Neovim path despite the asynchronous actor boundary.

The first renderer reconstructs one plain text viewport from the active
buffer's logical lines and exposes its mode, command line, latest message, and
cursor. It does not consume Neovim's external UI grid yet. Syntax highlighting,
line numbers, signs, folds, wrapping, conceal, extmarks, virtual text, split
windows, floating windows, and popup menus are therefore not represented.
Neovim still executes Lua that creates those features, but plugins whose
essential interface depends on them are loaded without a complete visual UI.

## Remote File Protocol

An open request names a workspace-relative path and may include a one-based
line and column. Absolute paths, parent traversal, control bytes, and oversized
paths are rejected before filesystem access. The current buffer bridge accepts
UTF-8 text without NUL bytes; other content is rejected rather than silently
rewritten. Consistent CRLF files are opened with Neovim's `dos` file format so
their line endings survive an ordinary edit and save.

The backend read primitive opens the target read-only even though the actor
also holds write authority. It accepts only an existing regular file and
rejects a final symlink, directory, FIFO, socket, device, or other special
target before reading. Read-only source files can therefore be viewed; a save
still depends on backend directory and replacement permissions.

The backend captures an immutable file snapshot, hashes it with SHA-256, and
returns it in bounded chunks. The client attaches that revision to the Neovim
buffer. A save streams a replacement with the base revision; the backend
rereads the file immediately before commit and rejects the save if the
revision changed. Saves are serialized per normalized path within the actor,
and successful saves use a sibling temporary file and rename, so readers do
not observe a partially truncated file.

The revision check plus rename is not a filesystem-wide compare-and-swap:
another workspace writer can still race in the interval between the final
reread and rename. External writers that need stronger ordering must share a
higher-level workspace lock.

The protocol caps each file at 8 MiB, each backend chunk at 7168 raw bytes, and
each direction at four concurrent transfers. Requests carry IDs and save
chunks carry contiguous byte offsets so retries, stale replies, and
out-of-order data cannot be mistaken for the active edit. Malformed or
out-of-order continuation messages terminate their snapshot or save transfer
and release the corresponding bounded slot. The client may retain up to four
file-sized transfer payloads, for a 32 MiB aggregate file-state bound. If it
abandons a non-terminal open or save after validating a reply, it sends an
explicit abort before reusing the transfer slot.

## Messages Into Neovim

Other plugins address the session init actor, not a client process or Neovim
socket. `open-editor` starts the client actor if necessary, waits for its
runtime-ready message and completed file-authorization handshake, and then
forwards a canonical `editor-open` message. Multiple bounded startup requests
are retained in order.

The editor actor translates the request into an `open-content` operation only
after the remote snapshot is complete. The bridge retains the authoritative
workspace-relative path and backend revision. The `acwrite` buffer's Neovim
filename is only a synthetic local identity and is never used for backend
I/O; no remote absolute path is treated as a local path.

## Messages From Neovim

The injected Lua bridge contributes user commands and `<Plug>` mappings for
normal Brood workflows. Typed actions become ordinary actor messages:

- a diff action starts a session terminal with the backend's colorized Git
  diff, so the same action works for local and remote workspaces;
- a terminal action asks the backend session to start a terminal, optionally
  with an initial command; and
- an agent action submits work to the session's default `coding-agent-1` actor
  and opens its presentation pane.

The generic message command addresses an explicitly named actor through the
same bounded route. The client actor stamps the selected backend and session;
Lua code cannot forge a different realm. Exact command spellings and message
encoding live in the linked bridge and actor sources.

## Rendering And Lifetime

The Node runner and actor communicate over bounded JSON Lines streams. The
runner publishes screen revisions containing rows, cursor, mode, and buffer
status. The presentation actor draws those revisions with Brood graphics and
forwards key, text, paste, resize, and focus events back to Neovim.
Each encoded screen revision is capped at 12 KiB. If escaping or metadata would
cross that bound, the bridge trims low-priority message, path, command-line,
and row text while retaining the viewport shape and cursor state.
Paste text is retained before any bytes are sent and uses Neovim's
start/continue/end paste phases, so queue pressure rejects a whole paste
instead of applying only a prefix or splitting its undo transaction.
When the in-memory Neovim inbox reaches its high-water mark, the runner pauses
its input pipe with the next complete record retained and resumes after
Neovim drains space; accepted file and paste streams are not discarded.
In the other direction, lifecycle, open, save, and protocol-result messages
have reserved outbox capacity and a bounded retry queue ahead of ordinary Lua
actions. Lua also admits no more than four pending remote saves, matching the
client transfer bound.

The local Neovim process belongs to the presentation actor. Closing or losing
the presentation stops that process; reopening creates a new local editor and
rereads authoritative backend content. The backend file actor belongs to the
session and remains available for the lifetime of that session. A clean client
stop unregisters its presentation address so a later open cannot be delivered
to a stale actor.

Related:

- [[terminal-plugin]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[presentation-client]]
- [[../workflows/ssh-deploy]]
