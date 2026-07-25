---
status: current
type: reference
authority: implementation
last-reviewed: 2026-07-24
code-links:
  - crates/broodd/src/config.rs
  - crates/brood-cli/src/realm_init.rs
  - brood.toml
---

# Config Schema Reference Pointers

Exact config schema behavior is defined by the deserializers in `crates/broodd/src/config.rs`.

Current top-level config families:

- `init`
- `client`
- `session`
- `module`
- `startup`

Current supported module kinds load self-contained WASM plugin artifacts:

- `plugin`
- `wasm-plugin`

For Component and Preview 1 artifacts, `<id>` is the stable hosted actor
instance ID. Different IDs may load the same artifact concurrently, and
reloading one ID replaces only that actor generation. Actor IDs must not
collide with built-in, legacy, or other hosted actors.

For a legacy core-module artifact, `<id>` remains a configuration label. The
artifact keeps its declared actor/provider names and singular replacement
lane; it does not claim the label as a hosted actor ID.

The optional `[module.<id>.resources]` table accepts
`filesystem-watches` and `filesystem-watch-events`. Omitted fields retain the
daemon ceilings; supplied values must be nonzero and no greater than those
ceilings. Runtime status reports live logical handles, configured capacities,
and monotonic overflow/rescan counters without exposing capability-root paths.

The parser recognizes the old `fast-key-handler` and `fast-map-hook` names only
to warn and skip them.

Target plugin manifests should eventually replace these startup entries with
declarations for actors, per-actor worker and I/O budgets, WASI/capability
grants, policy providers, actions, pane types, and publications. Workers
themselves are spawned dynamically inside actors rather than registered as
top-level plugin components.

Presentation layout is actor-owned client state, not a top-level backend config
family. The client root and key policy select presentation behavior; session
modules and startup applications remain server-owned.

Related:

- [[../systems/configuration]]
- [[wasm-fast-lanes]]
- [[../systems/plugin-runtime]]
