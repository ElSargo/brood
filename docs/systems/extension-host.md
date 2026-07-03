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
- A built-in lightweight plugin actor named `debug-foot`.
- Core-WASM fast lanes for key handling, map hooks, and test harnesses.

The target design is broader: plugins should use host-owned actors, mailboxes, registry entries, capability checks, sync policy lanes, and swapchains. Future APIs should be workspace/session/pane/input/layout/capability oriented and should not expose compositor internals.

## Current WASM Lanes

Current runnable WASM lanes are prototype lanes, not the final product API:

- fast key handler
- fast map hook
- async test harness

See [[../reference/wasm-fast-lanes]] for ABI details.

The key and map lanes prove that some host decisions need an immediate synchronous answer. The target preserves that property with sync policy lanes that read immutable published state and return bounded decision envelopes plus message batches. Direct side-effect-style host imports should become compatibility shims rather than the durable boundary.

## Target Plugin Host Shape

The target extension host should provide:

- async plugin actors addressed through host-provided mailboxes
- a registry for actor identities, services, actions, pane types, and capabilities
- named swapchains for latest immutable state publication
- sync policy lanes for immediate key, focus, map, and layout decisions
- host-owned command execution, layout validation, input routing, pane lifecycle, and native UI rendering

Plugin actors may be multi-threaded internally, but host-visible communication remains message passing. Sync policy lanes are not actors and must not block on actor queues.

## Hot Replacement

`manifold plug` currently hot-loads a WASM module into a running session. Auto mode tries the fast key-handler ABI first, then the fast map-hook ABI. Guest memory is not preserved across replacement.

Target hot reload should restart affected actors and sync lanes, preserve host-owned pane identity where possible, invalidate published swapchain frames by generation, and show reload failures in status output without tearing down the session process.

## Current DBus Extension Surfaces

The DBus extension surface supports extension registration, status publication, agent registration, plugin actor inspection/messaging, and related UI integration. The GTK session switcher uses these surfaces for status and agent rows.

The first actor prototype is `debug-foot`. It runs as a Tokio task with a bounded mailbox. Sending any message to it through `org.manifold.Plugin.SendMessage` or `manifold ctl actors send debug-foot <message>` enqueues mailbox work; the actor then asks the host to spawn `foot` in the active session.

## Related Pages

- [[../concepts/plugin]]
- [[../concepts/wasm-controller]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[layout-policy]]
- [[plugin-ui]]
- [[dbus-control]]
- [[configuration]]
