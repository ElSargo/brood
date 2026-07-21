# 0007: Local Presentation Uses Shared Memory And Client-Owned Actors

## Status

Accepted as the target presentation connection model.

## Context

Manifold must support both a very low-latency same-device presentation client
and remote clients without creating separate session, pane, lease, or actor
models. The existing client uses DBus only for startup and protocol-version
handshake. A proposed dedicated Unix-socket protocol would add another local
message carrier alongside DBus, shared memory, and DMA-BUF resources.

The presentation client also hosts presentation-scoped actors. Input policy,
layout, SDK UI state, canvas drawing, animation, and actor-to-actor messages
should remain endpoint-local. Only managed-window input, session actor messages,
and backend capability operations need to cross the presentation connection.

## Decision

The local connection has two planes:

1. DBus discovers or activates `manifoldd`, authenticates the initial endpoint,
   establishes a connection generation, exchanges size-sealed shared-memory and
   event descriptors, and brokers infrequent external-resource descriptors.
2. Bounded bidirectional shared-memory lanes carry all ordinary steady-state
   protocol traffic after bootstrap. The baseline uses independent
   single-producer/single-consumer directions, reserved critical capacity,
   reliable ordered records, coalescible latest-state records, explicit
   backpressure, and event notifications.

The target local profile does not add a dedicated Unix-socket message carrier.
DBus is not used per input event or per frame. DMA-BUF is a negotiated local
surface transport: reusable buffer pools and synchronization timelines are
registered through the descriptor broker, then shared-memory frame records
refer to generation-scoped resource IDs and timeline points. Copied images
remain the correctness fallback.

Remote clients use the same transport-independent semantic lanes over an
authenticated encrypted network carrier. QUIC is the initial target because it
provides multiplexed reliable streams, datagrams, congestion control, security,
and liveness. Remote surface subscriptions use per-window encoded or
damage-based transports rather than DMA-BUF. The final remote carrier and frame
codecs remain subject to workload testing.

Presentation actors run in the client for both local and remote connections.
Their local mailboxes, policy decisions, viewport state, SDK component state,
draw packets, and render updates do not traverse shared memory or the network.
Session actors remain in `manifoldd`; cross-runtime messages and backend
capability operations are asynchronous protocol traffic.

## Consequences

- The protocol model defines semantic critical, reliable, latest-state, bulk,
  and frame-resource lanes independently of their carrier.
- The shared-memory ABI requires explicit versioning, bounds, memory ordering,
  sequence validation, wakeup behavior, saturation policy, and crash cleanup.
- DBus owner loss and optionally pidfds provide local peer-liveness signals.
- File descriptors never appear as raw shared-memory values. Descriptor
  registration completes before a resource ID can appear in a ring record.
- Connection generations invalidate old mappings, leases, subscriptions,
  operations, frame resources, and input state after reconnect.
- DMA-BUF format, modifier, DRM-device, WGPU-import, and synchronization
  failures fall back to copied images without changing pane identity.
- Remote congestion cannot move layout, input policy, client actors, chrome, or
  final composition back into the backend.

## Verification Requirement

A separate client and daemon must sustain real input and managed-window frame
traffic over shared memory after a DBus bootstrap, preserve critical delivery
under latest-state saturation, reject stale resources after either endpoint
restarts, and release every registered frame resource on disconnect. A remote
proof must run the same client actors and viewport model while replacing the
local carrier and surface transport.
