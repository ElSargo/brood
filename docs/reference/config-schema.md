---
status: current
type: reference
authority: implementation
last-reviewed: 2026-07-03
code-links:
  - crates/manifoldd/src/config.rs
  - manifold.toml
---

# Config Schema Reference Pointers

Exact config schema behavior is defined by the deserializers in `crates/manifoldd/src/config.rs`.

Current top-level config families:

- `session`
- `module`
- `startup`

Current module kinds are prototype WASM lanes:

- `fast-key-handler`
- `fast-map-hook`

Target plugin manifests should eventually replace these lane-specific kinds with declarations for actors, sync policy lanes, capabilities, actions, pane types, and swapchains.

Current startup realms:

- `session`
- `presentation`

Related:

- [[../systems/configuration]]
- [[wasm-fast-lanes]]
- [[../systems/plugin-runtime]]
