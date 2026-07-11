---
status: current
type: reference
authority: implementation
last-reviewed: 2026-07-10
code-links:
  - crates/manifoldd/src/config.rs
  - manifold.toml
---

# Config Schema Reference Pointers

Exact config schema behavior is defined by the deserializers in `crates/manifoldd/src/config.rs`.

Current top-level config families:

- `session`
- `layout`
- `module`
- `startup`

Current supported module kinds load self-contained WASM plugin artifacts:

- `plugin`
- `wasm-plugin`

The parser recognizes the old `fast-key-handler` and `fast-map-hook` names only
to warn and skip them.

Target plugin manifests should eventually replace these startup entries with
declarations for actors, per-actor worker and I/O budgets, WASI/capability
grants, policy providers, actions, pane types, and publications. Workers
themselves are spawned dynamically inside actors rather than registered as
top-level plugin components.

Current startup realms:

- `session`
- `presentation`

Related:

- [[../systems/configuration]]
- [[wasm-fast-lanes]]
- [[../systems/plugin-runtime]]
