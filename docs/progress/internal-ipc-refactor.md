---
status: progress
type: evidence
authority: implementation
---

# Internal IPC Refactor

On 2026-08-23, local presentation bootstrap and steady-state backend operations
moved off D-Bus. D-Bus remains available as the public application-facing API.
The applicable target is [[../targets/systems/presentation-transport]].

## Implemented Boundary

The daemon owns a private mode-`0600` Unix `SOCK_SEQPACKET` endpoint beneath a
mode-`0700` runtime directory. It validates the presentation protocol version
and transfers bootstrap, event, frame-resource, and shared-memory descriptors
with `SCM_RIGHTS`. Connections persist across requests and are discarded after
any send or receive failure.

Presentation control and surface-session operations use separate request and
response memfd rings with eventfd wakeups. The rings carry correlated
`brood-protocol` backend values. They do not share storage with actor mailboxes,
so control traffic cannot consume actor capacity or inherit the actor record's
routing strings and large-record copy. Each direction has four 64 KiB records
and maps 262,784 bytes, compared with the actor lane's sixteen records and
approximately 1 MiB mapping.

The local presentation client and the network relay both use the same backend
request/response ABI. The relay translates between the network reliable lane
and local shared memory without calling back through D-Bus. Realm artifact
installation and descriptor/resource exchange use the private packet endpoint.

The network proof exposed two JSON-oriented enum encodings and three omitted
optional fields inside the Postcard catalog wire value. Human-readable JSON
keeps its tagged enum representation, while the binary carrier now uses an
externally tagged representation and always emits positional struct fields.
Protocol version 24 covers that wire correction.

The public D-Bus interface no longer exposes presentation fast-path creation,
generation-local frame-resource lookup, or realm-artifact queue management.
External application control, manager, extension, plugin, agent, status,
compositor, and diagnostic methods remain.

## Verification

The Nix-backed focused suites passed 200 tests across `brood-protocol`,
`brood-transport`, `brood-ipc`, `brood-cli`, `broodd`, `brood-client`, and
`brood-remote`. This includes binary catalog round-trip, packet-boundary and
descriptor-passing coverage over a persistent connection, backend-ring
capacity/wrap coverage, the exact backend-ring mapping size, and private socket
discovery with a nested bus address. Focused Clippy with warnings denied passed
for `brood-protocol`, `brood-transport`, `brood-ipc`, `brood-cli`, and
`brood-remote`.

The packaged nested behavior workload reached all refactor-sensitive stages:

- protocol-version 24 private bootstrap;
- terminal rendering and real keyboard input;
- presentation disconnect while the daemon-owned shell stayed alive;
- presentation reconnection with the same shell content;
- terminal color and environment assertions;
- first-party status/control requests through the private packet path.

The retained evidence includes `terminal-initial.png`,
`terminal-reconnected.png`, both presentation-client logs, and detached/live
status snapshots. The complete checked-in workload still exits later when its
Ghostty managed-window phase asks the headless wlroots compositor for a backend
DRM FD. That failure occurs after the IPC, disconnect, and reconnect assertions;
it is not recorded as a complete nested-suite pass.

The checked-in network behavior script crossed the refactor-sensitive path and
reported `OK remote transport`: it decoded the catalog, executed a backend
status request through the local shared-memory bridge, transported a real frame,
and exchanged artifacts in both directions. The script still exits afterward
when a separately downloaded presentation WASM actor times out in its lifecycle
hook. That later actor-runtime failure is not recorded as a complete script
pass, but the network-to-local IPC bridge is accepted by the proof's own
transport marker.
