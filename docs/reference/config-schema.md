---
status: current
type: reference
authority: implementation
last-reviewed: 2026-07-01
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

Current module kinds:

- `fast-key-handler`
- `fast-map-hook`

Current startup realms:

- `session`
- `presentation`

Related:

- [[../systems/configuration]]
- [[wasm-fast-lanes]]
