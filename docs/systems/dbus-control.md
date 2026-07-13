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

## Session Bus Services

Each managed surface-session realm owns an independent `BusRouter`. Socket
connections receive typed `ConnectionId` values, and connection lifecycle,
unique-name allocation, well-known-name acquisition/replacement/release, and
disconnect cleanup cross fallible state-transition methods. Rejected
transitions do not mutate owner tables. Successful ownership changes produce
explicit `NameOwnerChange` values before they are encoded and broadcast.

Parsed method calls, method returns, errors, and signals become a
`MessageClass` plus a destination. The router returns a typed unicast,
broadcast, unknown-destination, or drop decision. Byte delivery is a separate
fallible boundary: closed client channels are reported rather than silently
discarded. Built-in daemon and notification owners have explicit unique
identities, while per-realm owner tables are never shared.

The virtual notification owner converts host proxy results and host signals
into typed success/failure and forwarding transitions. Only signals matching
the originating host notification id return to that managed client; a matching
close event terminates its forwarder. This is a focused local broker and does
not claim hostile-code or internet-safe isolation.

The wire boundary is independent from that future routing design. Socket input
must pass through a fixed-header type that validates the endian marker,
eight-byte body alignment, exact frame length, checked arithmetic, and a bounded
allocation before zbus sees the bytes. Rebuilding a forwarded message accepts
only a body and signature obtained from the same successfully parsed zbus
message. The two zbus unsafe constructors remain private to these audited
helpers; routing code operates on typed `Message` values.

`tools/test-typed-session-bus-nested.sh` exercises those layers through the
built-in offscreen nested supervisor. The real GTK session switcher visibly
shows two surface sessions, clients on their distinct realm addresses observe
different ownership for the same service name, and four managed Python DBus
clients exchange introspection, properties, methods, replies, and signals. The
last client sends a substantive notification through its ordinary managed
session bus and records the real host notification id in a readable transcript
beside the rendered screenshot.

## Related Pages

- [[../reference/dbus-interfaces]]
- [[runtime]]
- [[../workflows/enter-session]]
