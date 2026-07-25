---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-24
code-links:
  - crates/broodd/src/config.rs
  - brood.toml
---

# Configuration

Brood configuration is TOML. The current checked-in config is `brood.toml`; user config is discovered through the normal XDG path first.

Exact parsing behavior lives in `crates/broodd/src/config.rs`.

## Config Discovery

Current discovery order:

- `BROOD_CONFIG`, when set and non-empty
- `$XDG_CONFIG_HOME/brood/brood.toml`
- `~/.config/brood/brood.toml`
- nearest checked-in `brood.toml` found by walking upward from the running binary

## Config Sections

Current section families:

- `[init]` for the daemon, session, and client realm root actors
- `[client.key-policy]` for the presentation client's synchronous policy provider
- `[module.<id>]` for startup modules
- `[startup.<id>]` for startup hosted apps
- `[session.runner]` for project environment activation

Current supported module kinds load self-contained WASM plugin artifacts:

- `plugin`
- `wasm-plugin`

`[init]` selects `backend`, `session`, and `client` Component Model artifacts.
Relative paths resolve from the config directory. The backend artifact is
instantiated once for the daemon even when there are no surface sessions. The
session artifact is instantiated once for every surface session, while the
client artifact is instantiated once for the presentation process. Once a root
is selected, it owns that realm's actor startup policy and, for a session root,
app startup policy through the actor lifecycle protocol; legacy
module and startup-app loading remains a compatibility fallback only for realms
without a configured root. Exact recovery and override flags remain in the CLI
definitions.

`[client.key-policy]` selects the provider export and core-WASM artifact used
before the client routes a key to either a local plugin pane or a managed
window. Relative artifact paths resolve from the config directory. A CLI
selection remains an explicit recovery override; normal `brood` startup
uses this config selection without wrapper arguments.

The `<id>` in `[module.<id>]` is also the hosted actor instance ID for Component
and Preview 1 modules. Multiple entries may therefore instantiate the same
artifact independently. Replacement and shutdown are per ID; one actor's
workers and resources are joined without removing its peers.

Configured hosted actors may request `filesystem:workspace-read` or
`filesystem:workspace-write` in `capabilities`. The realm host preopens its
workspace at guest `.` with the requested access. Client-init children use the
presentation client's launch directory; session actors use the active
session's working directory. Without one of those grants, a hosted actor keeps
the narrow package-directory, read-only default.

Runtime I/O authority is also explicit. `subprocess:<name>` admits one
PATH-resolved command name, `pty` admits real pseudoterminal attachment for an
otherwise-allowed command, and `network:loopback` admits outbound connections
to loopback peers. These grants survive replacement and workspace rebind with
the configured instance ID; they do not imply arbitrary process, terminal, or
network authority.

Hosted plugin watch budgets are declared in `[module.<id>.resources]`.
`filesystem-watches` bounds live logical watch handles and
`filesystem-watch-events` bounds queued events per handle before the queue is
replaced by a reconciliation marker. Both values must be nonzero and may only
narrow the daemon defaults (16 watches and 4096 events). The same typed limits
apply to Component and Preview 1 generations and survive replacement and
workspace-session rebind under the configured actor ID.

Hosted plugins use `[module.<id>.lifecycle]` as the target lifecycle source of
truth. `data` is a string-keyed map delivered as typed, ordered start-context
entries; `start-timeout-ms` and `stop-timeout-ms` bound explicit guest
completion of each hook. The context also supplies instance ID, generation,
start reason, surface-session ID, and guest capability root. The root is always
guest-relative and never reveals the host path. Timeouts must be non-zero.

`startup-message` remains a legacy bootstrap convenience for plugins without a
lifecycle table. Combining it with `lifecycle` is rejected rather than running
two competing startup mechanisms. Exact parser spelling and defaults remain in
[`config.rs`](../../crates/broodd/src/config.rs).

For legacy core-module artifacts, the config table key remains only a module
label. Those artifacts keep their declared actor/provider names and singular
replacement lane. Runtime `brood plug --id` rejects that legacy case rather
than reporting an actor ID that was never registered.

The old `fast-key-handler` and `fast-map-hook` names are rejected with a legacy
warning rather than loaded.

The target plugin configuration should move from these startup entries to
plugin manifests that declare actors, per-actor worker and I/O budgets,
WASI/capability grants, policy providers, actions, pane types, and publications.
Workers remain dynamically scoped to actors rather than top-level manifest
components.

Session startup applications remain server-owned. Presentation applications
and wrapper layouts are started by the client root actor, so no backend startup
entry can acquire viewport placement.

## Related Pages

- [[../reference/config-schema]]
- [[runtime]]
- [[plugin-runtime]]
- [[../concepts/session-runner]]
