---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-03
code-links:
  - crates/manifoldd/src/wasm_key.rs
  - crates/manifoldd/src/wasm_map.rs
  - crates/manifoldd/src/wasm_test.rs
  - crates/manifoldd/src/control.rs
---

# Extension Host

The current extension story has three implementation layers:

- DBus extension/status/agent/plugin surfaces for external tools and debugging.
- A built-in lightweight plugin actor named `debug-toggle` plus a prototype sync key lane.
- Core-WASM fast lanes for key handling, map hooks, and test harnesses.

The target design is broader: plugins should use host-owned actors, mailboxes, registry entries, capability checks, policy providers, and publications. Future APIs should be workspace/session/pane/input/layout/capability oriented and should not expose compositor internals.

## Current WASM Lanes

Current runnable WASM lanes are prototype lanes, not the final product API:

- fast key handler
- fast map hook
- async test harness

See [[../reference/wasm-fast-lanes]] for ABI details.

The key and map lanes prove that some host decisions need an immediate synchronous answer. The target preserves that property with policy providers that read immutable published state and return bounded decision envelopes plus message batches. Direct side-effect-style host imports should become compatibility shims rather than the durable boundary.

## Target Plugin Host Shape

The target extension host should provide:

- async plugin actors addressed through host-provided mailboxes
- a registry for actor identities, services, actions, pane types, and capabilities
- named publications for latest immutable state publication
- policy providers for immediate key, focus, map, and layout decisions
- host-owned command execution, layout validation, input routing, pane lifecycle, and native UI rendering

Plugin actors may be multi-threaded internally, but host-visible communication remains message passing. Policy providers are not actors and must not block on actor queues.

## Hot Replacement

`manifold plug` currently hot-loads a WASM module into a running session. Auto mode tries the fast key-handler ABI first, then the fast map-hook ABI. Guest memory is not preserved across replacement.

Target hot reload should restart affected actors and policy providers, preserve host-owned pane identity where possible, drop or replace publication frame roots for the reloaded components, and show reload failures in status output without tearing down the session process.

## Current DBus Extension Surfaces

The DBus extension surface supports extension registration, status publication, agent registration, plugin actor inspection/messaging, and related UI integration. The GTK session switcher uses these surfaces for status and agent rows.

The first actor prototype is `debug-toggle`. It runs as a Tokio task with a bounded default mailbox slot. The actor publishes an immutable boolean frame to a host-owned publication. `Ctrl+O` is handled by a prototype sync key lane that reads that frame, asks the host to spawn `foot htop` or `foot btop`, and returns a mailbox message asking the actor to toggle and publish the next frame. Sending any message through `org.manifold.Plugin.SendMessage`, `org.manifold.Plugin.SendMessageToSlot`, `manifold ctl actors send debug-toggle <message>`, or `manifold ctl actors send-slot debug-toggle default <message>` also toggles the actor frame.

## Related Pages

- [[../concepts/plugin]]
- [[../concepts/wasm-controller]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[layout-policy]]
- [[plugin-ui]]
- [[dbus-control]]
- [[configuration]]
