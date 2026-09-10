# Plugins

`workspace-file-log` is a complete Preview 1 actor-and-worker plugin. It watches
the active workspace through ordinary WASI filesystem calls and records changed
file names in `.brood/workspace-file-log.log`, including reversible native
path-byte escapes for names that are not UTF-8.

Root-level target-shape Brood plugins live here.

- `default-workspace/` ports the existing prototype key handler and map/layout hook into the new `brood-wasm` ABI shape: exported policy providers plus guest arena allocation.
- `session-switcher/` is the default client-realm shell plugin: its init actor starts a native plugin pane which renders the live session catalogue and performs typed create, switch, rename, and close operations.
- `status-bar/` is persistent presentation-global shell chrome with session
  context and the essential shortcut-discovery hints.
- `theme-switcher/` opens the shared picker with all bundled and user Helix themes, previews focus, and saves selection (Ctrl+Alt+H).
- `picker/` is the reusable client-realm fuzzy picker actor. It serializes concurrent requests, accepts streamed items through stable IDs, and returns the original selected result and index.
- `inbox/` is the presentation-global completion queue revealed on demand. It
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

Component Model plugins use `crates/brood-plugin-sdk`. The SDK owns the
generated `brood-plugin` bindings plus common actor, input, graphics-resource,
text-bounding, and worker-runtime mechanisms. Plugins remain standalone crates
so their dependency closures and artifacts stay independent; the SDK does not
own their protocols, state machines, supervision, or UI composition.

Preview 1 core modules (`default-workspace` and `workspace-file-log`) retain
their lane-specific ABI. They share a host contract with Component plugins but
do not pretend to implement the Component Model `Guest` interface.

## Visual language

Native plugin canvases use the shared Ashen-derived tokens in
`brood_graphics::theme::ashen`. New surfaces should map semantic roles to those
tokens instead of defining a separate palette. Dense utility surfaces follow
the diff viewer's layout language: compact headers and footers, flat list rows,
subtle separators, and a narrow accent stripe for selection. Pointer-driven
controls expose the same interaction cycle everywhere: a raised hover surface,
an accent-tinted pressed surface, and activation only when the primary button
is released over the control that received the press.

`component-workspace-watch` is the Component Model filesystem-watch proof. It
contains the current small guest-side typed-resource wrapper and maintains a
human-readable workspace change artifact with reversible native path-byte
escapes; its README links the nested proof.
