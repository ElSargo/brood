---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-10
code-links:
  - crates/manifoldd/src/config.rs
  - manifold.toml
---

# Configuration

Manifold configuration is TOML. The current checked-in config is `manifold.toml`; user config is discovered through the normal XDG path first.

Exact parsing behavior lives in `crates/manifoldd/src/config.rs`.

## Config Discovery

Current discovery order:

- `MANIFOLD_CONFIG`, when set and non-empty
- `$XDG_CONFIG_HOME/manifold/manifold.toml`
- `~/.config/manifold/manifold.toml`
- nearest checked-in `manifold.toml` found by walking upward from the running binary

## Config Sections

Current section families:

- `[module.<id>]` for startup modules
- `[startup.<id>]` for startup hosted apps
- `[session.runner]` for project environment activation

Current supported module kinds load self-contained WASM plugin artifacts:

- `plugin`
- `wasm-plugin`

The old `fast-key-handler` and `fast-map-hook` names are rejected with a legacy
warning rather than loaded.

The target plugin configuration should move from these startup entries to
plugin manifests that declare actors, per-actor worker and I/O budgets,
WASI/capability grants, policy providers, actions, pane types, and publications.
Workers remain dynamically scoped to actors rather than top-level manifest
components.

Startup apps can target the session realm or presentation realm. Presentation startup apps remain available when an auto-started global control server skips creating its own initial surface session.

## Related Pages

- [[../reference/config-schema]]
- [[runtime]]
- [[plugin-runtime]]
- [[../concepts/session-runner]]
