---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-01
code-links:
  - crates/manifoldd/src/wasm_key.rs
  - crates/manifoldd/src/wasm_map.rs
  - crates/manifoldd/src/wasm_test.rs
  - crates/manifoldd/src/control.rs
---

# Extension Host

The current extension story has two layers:

- DBus extension/status/agent surfaces for external tools.
- Core-WASM fast lanes for key handling, map hooks, and test harnesses.

The target design is broader than the current lanes. Future APIs should be workspace/session/pane/capability oriented and should not expose compositor internals.

## Current WASM Lanes

- fast key handler
- fast map hook
- async test harness

See [[../reference/wasm-fast-lanes]] for ABI details.

## Hot Replacement

`manifold plug` hot-loads a WASM module into a running session. Auto mode tries the fast key-handler ABI first, then the fast map-hook ABI. Guest memory is not preserved across replacement.

## Current DBus Extension Surfaces

The DBus extension surface supports extension registration, status publication, agent registration, and related UI integration. The GTK session switcher uses these surfaces for status and agent rows.

## Related Pages

- [[../concepts/wasm-controller]]
- [[dbus-control]]
- [[configuration]]
