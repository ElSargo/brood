---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-01
code-links:
  - crates/manifoldd/src/control.rs
  - crates/manifoldd/src/session_bus.rs
---

# DBus And Control

Manifold exposes DBus interfaces for control, session management, extension registration, status publication, agent rows, compositor activation, and notification forwarding.

Exact method signatures are defined in `crates/manifoldd/src/control.rs`.

## Current Interfaces

- `org.manifold.Session.Control`
- `org.manifold.Manager`
- `org.manifold.Extension`
- `org.manifold.Agent`
- `org.manifold.Status`
- `org.manifold.Compositor`

## CLI Control

`manifold ctl` sends command argv to a DBus control endpoint. It can also send a raw JSON argv array with `--json`.

Common control requests include status, status-json, spawn, spawn-in-session, focus, plug, harness signal, nested-inner proxying, and quit.

## Host Activation

Presentation focusing can use activation-token-aware compositor calls where available. The switcher app uses the compositor DBus surface to avoid stealing focus incorrectly after session changes.

## Session Bus Direction

The session-local DBus bus/proxy model is still a staged direction. Current code contains session-bus support and notification forwarding pieces, but internet-safe or hostile-code isolation is not a current claim.

## Related Pages

- [[../reference/dbus-interfaces]]
- [[runtime]]
- [[../workflows/enter-session]]
