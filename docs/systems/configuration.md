---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-11
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

The `<id>` in `[module.<id>]` is also the hosted actor instance ID for Component
and Preview 1 modules. Multiple entries may therefore instantiate the same
artifact independently. Replacement and shutdown are per ID; one actor's
workers and resources are joined without removing its peers.

Configured hosted actors may request `filesystem:workspace-read` or
`filesystem:workspace-write` in `capabilities`. The daemon preopens the active
session's working directory at guest `.` with the requested access. Without one
of those grants, the hosted actor keeps the narrow package-directory,
read-only default.

Hosted plugins use `[module.<id>.lifecycle]` as the target lifecycle source of
truth. `data` is a string-keyed map delivered as typed, ordered start-context
entries; `start-timeout-ms` and `stop-timeout-ms` bound explicit guest
completion of each hook. The context also supplies instance ID, generation,
start reason, surface-session ID, and guest capability root. The root is always
guest-relative and never reveals the host path. Timeouts must be non-zero.

`startup-message` remains a legacy bootstrap convenience for plugins without a
lifecycle table. Combining it with `lifecycle` is rejected rather than running
two competing startup mechanisms. Exact parser spelling and defaults remain in
[`config.rs`](../../crates/manifoldd/src/config.rs).

For legacy core-module artifacts, the config table key remains only a module
label. Those artifacts keep their declared actor/provider names and singular
replacement lane. Runtime `manifold plug --id` rejects that legacy case rather
than reporting an actor ID that was never registered.

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
