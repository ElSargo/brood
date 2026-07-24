---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-24
code-links:
  - crates/manifoldd/src/control.rs
  - crates/manifoldd/src/browser.rs
  - crates/manifoldd/src/session_bus.rs
---

# DBus And Control

Manifold exposes DBus interfaces for server discovery and presentation
bootstrap, session management, external tool registration, status publication,
agent rows, and notification forwarding.

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
not interpret positional token indexes.

## Presentation Bootstrap

DBus may discover or start the session server, exchange descriptors for the
local presentation protocol, and carry asynchronous control commands. The
available window and session catalog is published through the server-to-client
shared-memory state lane; it is not polled over DBus. Viewport focus, placement,
selected membership, layout, and switcher visibility are not daemon DBus
operations.

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

Session-bus proofs use clients on distinct realm addresses. They observe
different ownership for the same service name, exchange introspection,
properties, methods, replies, and signals, and route a substantive notification
through the managed session bus. These proofs do not depend on presentation
placement.

## Related Pages

- [[../reference/dbus-interfaces]]
- [[runtime]]
- [[../workflows/enter-session]]
