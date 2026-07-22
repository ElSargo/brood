---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-11
code-links:
  - crates/manifold-wasm/src/module.rs
  - crates/manifold-wasm/src/actor.rs
  - crates/manifold-wasm/src/hosted_actor.rs
  - crates/manifold-wasm/src/legacy_runtime.rs
  - crates/manifold-wasm/src/global_key_policy.rs
  - crates/manifold-wasm/src/policy_provider.rs
  - crates/manifold-wasm/src/component_actor.rs
  - crates/manifold-wasm/src/preview1_worker.rs
  - crates/manifold-wasm/src/completion_io.rs
  - crates/manifold-wasm/src/coordination.rs
  - crates/manifold-wasm/src/test_harness.rs
  - crates/manifoldd/src/wasm_test.rs
  - crates/manifoldd/src/control.rs
  - crates/manifoldd/src/state.rs
---

# Extension Host

The current extension story has four implementation layers:

- DBus extension/status/agent/plugin surfaces for external tools and debugging.
- A built-in lightweight plugin actor named `debug-toggle` plus a native host
  mailbox/publication scaffold.
- Core-WASM actors and synchronous key policy providers, plus the runtime-owned
  behavior-test harness engine and its thin daemon request adapter.
- Runnable Component Model and Preview 1 compatibility actor/worker prototypes.

The target design is broader: plugins use actors as host-visible isolation and
messaging domains, private actor-scoped workers for shared-memory concurrency,
completion-based actor I/O, standard WASI resources, policy providers, and
publications. Future APIs should be workspace/session/pane/input/layout/
capability oriented and should not expose compositor internals.

## Current WASM Lanes

Current runnable WASM lanes are prototypes, not the final product API. A
self-contained core module can declare named actors and synchronous policy
providers. The presentation client executes selected key and flat-layout
providers over its own pane inventory; the daemon retains the older key and
actor-driven layout paths for its built-in presenter compatibility path. The
separate core-WASM test harness remains for behavior-test orchestration.

See [[../reference/wasm-fast-lanes]] for ABI details.

The actor/worker prototype adds two artifact shapes:

- A Component Model actor owns one component instance and Store, a bounded
  default mailbox, a reserved completion lane, and cooperative async workers.
  Its Manifold WIT exposes completion reads, writes, cancellation, worker
  spawning, and explicit worker yield points. The runnable component imports no
  standard WASI surface, so its dispatcher cannot bypass completion I/O; host
  task identity gates the worker-only async imports.
- A marked `wasm32-wasip1-threads` compatibility module runs its dispatcher and
  actor-scoped workers in separate Stores over one imported shared memory.
  Workers receive standard Preview 1 WASI and may use ordinary blocking-looking
  calls such as Rust `std::io::Read`; the actor dispatcher has no blocking
  filesystem or stdin resources and uses the non-blocking completion imports.
  Descriptor tables remain Store-local even though memory and granted authority
  are shared.

The actor completion APIs in both prototypes use the same host completion
engine. Operations are bounded. A cancellation request reports queue admission;
if it wins before result delivery, the operation produces exactly one typed
terminal cancellation completion. Actor generations prevent a stale completion
from reaching a replacement actor.

Runnable Component and Preview 1 actors are registered by stable instance ID.
Configuration module IDs and `manifold plug --id` can host the same artifact
more than once; replacement is generation-scoped and does not stop unrelated
instances. Actor listing and mailbox diagnostics expose every entry. Legacy
core-module actor inventories retain their singular lane and declared export
names. Configuration table IDs remain labels for those artifacts, while custom
runtime instance IDs are rejected rather than being silently reported without
a matching registered actor.

The Component Model example currently uses only the native async worker file
calls. The Preview 1 compatibility example is the runnable proof that an
ordinary Rust `std::io::Read` can wait in a worker without blocking its actor;
the target can move that compatibility path to standard Component Model WASI
once worker/dispatcher context separation is representable and supported by
production guest toolchains.

The key lane proves that some host decisions need an immediate synchronous
answer. The target preserves that property with policy providers that read
immutable published state and return bounded decision envelopes plus message
batches. Layout does not need that synchronous lane: actors receive layout-state
messages and return host-validated layout commands. Direct side-effect-style
host imports should remain compatibility shims rather than the durable
boundary.

## Target Plugin Host Shape

The durable model is specified once in [[plugin-runtime]], with its resource
protocol in [[plugin-io-and-resources]]. In short: actors are public fault
domains, workers are private shared-memory tasks, policy providers are bounded
synchronous decisions, and installed plugins are trusted within the enclosing
session environment. This page only records how the current prototypes map to
that target.

## Hot Replacement

`manifold plug` hot-loads a WASM artifact into a running session. Plugin loading
recognizes the Preview 1 compatibility marker, falls back from core-module
inspection to Component Model loading, and otherwise discovers core-module
actor/policy exports. A key policy can also be selected explicitly. See the
parser and replacement routing in `control.rs` and `state.rs` for exact
selection behavior.

All runnable actor lanes apply structured
replacement. Individual Component worker cancellation is cooperative and emits
its terminal event only after the guest export returns; replacement terminates
the owning Store, then joins the runtime and completion supervisor before the
new generation becomes active. Preview 1 replacement aborts worker calls, drops
their Stores, and joins its completion supervisor. Guest memory is not migrated
to the replacement generation. Core-module sets are built against detached
prepared mailboxes. The old set remains the published owner until cancellation
has been observed and every task joined; the registry then activates candidate
names as one transition before starting their tasks. Candidate actor or policy
load failures leave the published actors and key policy unchanged.

Core-module actors opt into daemon events with typed marker exports associated
with their actor declaration. Pane mapping and layout snapshots are routed only
to matching subscribers, and activation queues a fresh layout snapshot so a
replacement converges without waiting for another client to map. Malformed,
unknown-event, and orphan markers reject the module. Exact marker spellings are
defined in `module_link.rs`.

Target hot reload should cancel affected workers and in-flight I/O before
restarting actors and policy providers, preserve host-owned pane identity where
possible, drop or replace publication frame roots for the reloaded components,
and show reload failures in status output without tearing down the session
process.

## Runnable Proof

`tools/test-wasm-component-worker.sh` drives the Component Model actor directly
and proves actor progress during pending worker I/O, completion delivery and
cancellation, cooperative CPU-worker yield, and dispatcher deadline
enforcement.

`tools/test-wasm-preview1-worker.sh` drives the low-level compatibility runtime
and its actor facade. `tools/test-wasm-preview1-worker-nested.sh` is the
end-to-end daemon proof: it uses the built-in nested test topology with an
offscreen outer presentation, reaches the actor through the normal control
surface, leaves an ordinary worker read pending while actor traffic and
completion I/O continue, feeds the worker, then hot-reloads while another read
is pending and verifies generation isolation and structured shutdown.

`tools/test-wasm-actor-lifecycle-nested.sh` loads the real
`wasm-actor-lifecycle-proof` core plugin beside two rendered Wayland clients.
Its subscribed actor first arranges columns, then a same-name hot replacement
arranges rows. An invalid middle candidate is rejected while the first actor
still accepts a real mailbox probe and retains its rendered columns. A second actor has no subscriptions and would visibly spawn an
unwanted client if routing regressed to broadcast; both screenshots retain
exactly the intended two panes.

These workloads establish the execution and I/O model. The prototype still
loads one actor artifact without package-manifest declarations, gives it a
temporary read-only grant rooted at the artifact directory, and does not yet
implement the complete target registry, publication, policy-provider, or SDK
surface described below.

## Current DBus Extension Surfaces

The DBus extension surface supports extension registration, status publication, agent registration, plugin actor inspection/messaging, and related UI integration. The GTK session switcher uses these surfaces for status and agent rows.

The first actor prototype is `debug-toggle`. It runs as a Tokio task with a
bounded default mailbox slot and publishes an immutable boolean frame to a
host-owned publication. Sending a message through the DBus plugin interface or
the equivalent actor control command toggles that frame. No current input path
consults it; the WASM global-key policy owns synchronous key decisions.

## Related Pages

- [[../concepts/plugin]]
- [[../concepts/wasm-controller]]
- [[plugin-runtime]]
- [[plugin-io-and-resources]]
- [[input-shortcuts]]
- [[layout-actor]]
- [[plugin-ui]]
- [[dbus-control]]
- [[configuration]]
