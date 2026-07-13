---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-13
code-links:
  - crates/manifoldd/src/control.rs
  - crates/manifoldd/src/browser.rs
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

Raw argv and JSON requests cross one fallible parsing boundary before they can
mutate compositor state. Command-specific parsers produce typed commands,
validate exact arity, and reject ambiguous option forms; execution code does
not interpret positional token indexes. `tools/test-typed-control-parser-nested.sh`
exercises both encodings in the offscreen nested harness and records the exact
rejections alongside a real Component actor loaded through the typed `plug`
command.

## Host Activation

Presentation focusing can use activation-token-aware compositor calls where available. The switcher app uses the compositor DBus surface to avoid stealing focus incorrectly after session changes.

## Session Bus Direction

The session-local DBus bus/proxy model is still a staged direction. Current code contains session-bus support and notification forwarding pieces, but internet-safe or hostile-code isolation is not a current claim.

The wire boundary is independent from that future routing design. Socket input
must pass through a fixed-header type that validates the endian marker,
eight-byte body alignment, exact frame length, checked arithmetic, and a bounded
allocation before zbus sees the bytes. Rebuilding a forwarded message accepts
only a body and signature obtained from the same successfully parsed zbus
message. The two zbus unsafe constructors remain private to these audited
helpers; routing code operates on typed `Message` values.

This encapsulation does not decide bus ownership, name policy, or cross-session
routing. Those remain session-bus service responsibilities rather than wire
parser concerns.

## Related Pages

- [[../reference/dbus-interfaces]]
- [[runtime]]
- [[../workflows/enter-session]]
