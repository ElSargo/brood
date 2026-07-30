# Plugins

`workspace-file-log` is a complete Preview 1 actor-and-worker plugin. It watches
the active workspace through ordinary WASI filesystem calls and records changed
file names in `.brood/workspace-file-log.log`, including reversible native
path-byte escapes for names that are not UTF-8.

Root-level target-shape Brood plugins live here.

- `default-workspace/` ports the existing prototype key handler and map/layout hook into the new `brood-wasm` ABI shape: exported policy providers plus guest arena allocation.
- `session-switcher/` is the default client-realm shell plugin: its init actor starts a native plugin pane which renders the live session catalogue and performs typed create, switch, rename, and close operations.
- `picker/` is the reusable client-realm fuzzy picker actor. It serializes concurrent requests, accepts streamed items through stable IDs, and returns the original selected result and index.
- `inbox/` is the always-visible, presentation-global completion queue. It
  activates the source presentation actor when a notification is clicked.
- `diff-viewer/` is the local-workspace project-diff pane. Remote editor diff
  actions use a backend terminal until the viewer has its own split
  backend/presentation protocol.
- `neovim-client/` owns the presentation-local Wasm Neovim process, input,
  rendering, and messaging bridge.
- `neovim-files/` owns bounded, revision-checked workspace reads and atomic
  saves on the selected backend.
- `neovim-runtime/` contains the JSONL Node/Emscripten runner and injected Lua
  bridge. It mirrors the client's Neovim config into the Wasm filesystem.

The WASM crates are currently standalone because `brood-wasm` is
host/runtime-only. Once guest-side helpers exist, plugins can depend on a guest
SDK instead of hand-writing ABI exports and JSON envelopes.

## Visual language

Native plugin canvases use the shared Ashen-derived tokens in
`brood_graphics::theme::ashen`. New surfaces should map semantic roles to those
tokens instead of defining a separate palette. Dense utility surfaces follow
the diff viewer's layout language: compact headers and footers, flat list rows,
subtle separators, and a narrow accent stripe for selection.

`component-workspace-watch` is the Component Model filesystem-watch proof. It
contains the current small guest-side typed-resource wrapper and maintains a
human-readable workspace change artifact with reversible native path-byte
escapes; its README links the nested proof.
