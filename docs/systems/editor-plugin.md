---
status: target
type: system
authority: product
last-reviewed: 2026-07-29
code-links:
  - plugins/neovim/init.lua
  - examples/wasm-init-actor/src/lib.rs
  - plugins/terminal-session/src/lib.rs
  - plugins/terminal-ui/src/lib.rs
  - plugins/session-switcher/src/lib.rs
---

# First-Party Neovim Plugin

The Brood editor is Neovim adapted to the actor and pane model, not a partial
Vim reimplementation. The packaged `brood-nvim` command executes real Neovim,
loads the Brood bridge, and then preserves the user's normal configuration and
plugin environment. Vim modes, motions, operators, text objects, registers,
macros, command-line editing, and extension APIs remain Neovim's.

## Ownership And Composition

Each Brood session has at most one editor session actor. That actor owns the
Neovim child, PTY, server socket, buffers, and editor lifetime. A normal
terminal presentation actor renders and drives the PTY. This reuses the
first-party terminal rather than adding editor grids, buffer state, or a Vim
key interpreter to the host.

The editor can ask Brood to compose related state as ordinary panes:

- the project diff viewer;
- a new terminal, optionally seeded with a command;
- a coding-agent pane associated with submitted work; and
- any plugin pane reached through an explicitly addressed actor protocol.

`Ctrl+Alt+E` asks the active session init actor to create or locate the editor.
The client init actor creates the presentation terminal only after the session
actor has reached its Neovim server through a successful RPC probe and reports
ready. Startup requests remain in a bounded queue until that handshake
completes. Focus and placement then use the same deferred pane and layout paths
as other plugins.

## Messages From Neovim

The bridge writes bounded control records to Neovim's PTY stderr:

```text
OSC 777 ; brood ; lowercase-hex(compact JSON) BEL
```

Only the editor-role terminal interprets this private control family. It
decodes and validates the record, stamps the owning session identity, and
turns it into an ordinary actor message. The sequence is control data and is
not rendered into the terminal grid.

The user-facing commands are `:BroodDiff`, `:BroodTerminal [command]`,
`:BroodAgent [prompt]`, and
`:BroodMessage {target} {json-or-string}`. The typed commands expose the common
workflows without requiring callers to know actor IDs. The generic command can
address a named actor in the same session realm or the attached presentation;
it cannot address the host, another backend, or the privileged presentation
orchestration actors used by the typed commands. Exact encoding, validation,
and bounds live in the linked Lua and terminal-session sources.

The bridge contributes `<Plug>` mappings only. Users choose their own leader,
mode, and physical bindings, and their existing Neovim configuration remains
authoritative.

## Messages Into Neovim

An `open-editor` request contains a workspace path and optional one-based line
and column. The session init actor starts the editor when absent, otherwise it
forwards to the existing editor actor. The editor actor serializes bounded
requests and invokes the packaged Neovim remote client against its private
server socket. Opening a file therefore reuses the session's buffers and undo
state rather than spawning another pane or process.

Other plugins should use the session init actor as the stable editor service
address. They do not discover the socket, write PTY bytes, or assume that a
presentation is attached. Probe, launch, and remote-open failures return through
actor messages so the presentation shell can expose them instead of treating a
blank or unreachable editor as ready.

## Detach And Failure Semantics

The pane is a view of session state, not the owner of that state. Closing the
editor pane, replacing a presentation client, or disconnecting a remote
presentation detaches the terminal UI. The Neovim process and server socket
remain owned by the session actor, and reopening attaches a presentation UI to
the existing editor.

The current product has one terminal subscriber per editor session. Reopening
therefore restores or replaces that one live view; simultaneous independent
views of the same PTY are not promised.

Exiting Neovim ends the editor child. Explicitly stopping the editor actor or
closing its Brood session also tears down the PTY and socket. Actor startup or
remote-control failure is reported to the presentation shell and must not
silently create a second editor.

## Packaging

Neovim, the bridge wrapper, ripgrep, and the Wayland clipboard helper are part
of the local runtime and development profiles. The portable SSH backend carries
its own editor command and runtime files and places its `bin` directory first
on the remote backend `PATH`, so the allowlisted actor command resolves to the
deployed editor rather than an unrelated host executable. User configuration is
still discovered normally on the machine where Neovim runs.

Related:

- [[terminal-plugin]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[presentation-client]]
- [[../workflows/ssh-deploy]]
