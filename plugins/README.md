# Plugins

`workspace-file-log` is a complete Preview 1 actor-and-worker plugin. It watches
the active workspace through ordinary WASI filesystem calls and records changed
file names in `.brood/workspace-file-log.log`, including reversible native
path-byte escapes for names that are not UTF-8.

Root-level target-shape Brood plugins live here.

- `default-workspace/` ports the existing prototype key handler and map/layout hook into the new `brood-wasm` ABI shape: exported policy providers plus guest arena allocation.
- `session-switcher/` is the default client-realm shell plugin: its init actor starts a native plugin pane which renders the live session catalogue and performs typed create, switch, rename, and close operations.
- `diff-viewer/` is the toggleable project-diff pane: it renders every changed file in one continuous split diff, loads Git state in a worker, and relies on ordinary workspace placement.

These crates are currently standalone because `brood-wasm` is host/runtime-only. Once guest-side helpers exist, plugins can depend on a guest SDK instead of hand-writing ABI exports and JSON envelopes.

`component-workspace-watch` is the Component Model filesystem-watch proof. It
contains the current small guest-side typed-resource wrapper and maintains a
human-readable workspace change artifact with reversible native path-byte
escapes; its README links the nested proof.
