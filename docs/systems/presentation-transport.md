---
status: canonical
type: system
security-boundary: process-and-network
runtime-boundary: process-and-network
wire-format: not-frozen
authority: product
last-reviewed: 2026-07-25
code-links:
  - crates/brood-protocol/src/lib.rs
  - crates/brood-ipc/src/local_protocol.rs
  - crates/brood-ipc/src/shared_image.rs
  - crates/brood-transport/src/network.rs
  - crates/broodd/src/control.rs
  - crates/broodd/src/frame_export.rs
  - clients/brood-client/src/main.rs
  - crates/brood-remote/src/main.rs
---

# Presentation Transport

## Purpose

Presentation behavior is defined once and carried by interchangeable connection
and frame implementations. Pane identity, catalog revisions, input, lifecycle,
actor messages, scenes, and resource ownership have the same meaning whether
the client runs beside the session server or on another machine.

Transport selection has two independent axes:

| Deployment | Message carrier | Frame transport |
| --- | --- | --- |
| Same machine with compatible GPU sharing | shared-memory lanes | DMA-BUF |
| Same machine without compatible GPU sharing | shared-memory lanes | shared-memory images |
| Different machines | mutually authenticated QUIC streams | encoded network images |

These are negotiated profiles, not separate presentation protocols. Adding a
carrier or frame codec must not create new layout, input, actor, or lifecycle
semantics.

## Semantic Protocol

`brood-protocol` is the source of truth for transport-neutral messages,
capability offers, profiles, frame metadata, and protocol versioning. Carrier
crates encode these types; they do not define parallel message models.

Traffic is assigned to semantic lanes:

- `Critical` carries lossless latency-sensitive transitions and lifecycle.
- `Reliable` carries ordered catalog, actor, and resource transitions.
- `LatestState` carries supersedable pointer, viewport, resize, and scene state.
- `Bulk` is reserved for bounded large non-frame payloads.
- `FrameResource` carries frame content independently from scene publication.

Independent flow control is required. A large or stalled frame may reduce
visual freshness, but it must not delay a key transition, disconnect, or
resource-lifecycle message. Every payload is bounded before allocation or
decode.

Connection establishment compares immutable endpoint capabilities and selects
one profile for the connection generation. Failure to agree on protocol
version, carrier, frame transport, or bounds fails the connection; it does not
silently reinterpret messages. Reconnection creates a new generation and
invalidates every old lease, resource, mapping, and in-flight operation.

## Local Carrier

DBus discovers and authenticates the local session server and brokers sealed
memory and event descriptors. Steady-state messages use bounded shared-memory
records with event notifications. Local records are a codec for
`brood-protocol` messages rather than a second protocol definition.

Descriptor values never appear as raw integers in shared memory. DBus registers
a generation-scoped external resource first; records then refer to its stable
resource ID. Owner loss and mapping validation provide local peer-liveness and
stale-generation rejection.

## Local Frames

DMA-BUF is preferred when export, import, format, modifier, device, and
synchronization requirements are mutually supported. Failure in any of those
requirements selects the copied-image profile without changing pane or scene
identity.

The copied-image profile uses size-sealed memory containing immutable image
metadata and a reusable pixel allocation. The producer finishes all pixel
writes before publishing a release-ordered sequence. The consumer
acquire-loads that sequence before reading and uploads the image to its local
GPU. Reusing an allocation is allowed only under the connection's bounded
in-flight ownership rules.

## Network Carrier

The network carrier uses a mutually authenticated TLS-over-QUIC connection.
Each endpoint validates the other's configured certificate. Each semantic lane
owns an independently ordered stream pair, so transport-level head-of-line
blocking is contained to that lane. A handshake exchanges capabilities,
selects the remote profile, and assigns the server's connection generation
before ordinary traffic begins. Transport keepalives preserve an otherwise
idle static desktop. Stream readers remain owned by dedicated receive tasks so
selecting between lanes cannot cancel a partial framed read.

Network peers exchange semantic messages with length-delimited bounded
encoding. The baseline frame-resource packet is a losslessly compressed full
window image with explicit pane, resource, sequence, extent, stride, pixel
format, and color space. It establishes correctness and portability. Future
damage, video, or GPU-aware codecs may replace that payload after negotiation;
they must preserve scene/resource lifecycle and must not introduce a second
presentation model.

A relay may adapt a local session-server connection into the network carrier.
The relay is a transport endpoint: it neither computes layout nor hosts
presentation actors. The native remote client remains responsible for actors,
input policy, final composition, and acknowledgements. Catalog and actor
traffic retain their normal reliable ordering. Client backend and session
operations use correlated request/response messages on that same reliable
lane, so a remote presentation never reaches back to the server's local DBus
address.

## Frame And Scene Lifecycle

A scene is lightweight latest state and refers to generation-scoped frame
resources. Frame bytes or external descriptors travel on the frame-resource
path. A client may receive either first and joins them by generation, resource,
and sequence.

The client acknowledges presentation or explicitly releases superseded
resources. Disconnect uses a bounded critical-lane handshake: the client sends
its disconnect request and finishes that stream, the server drains it before
acknowledging and finishing the return stream, and the client drains the
acknowledgement before closing the connection. Completion revokes all
connection-owned resources. Producers retain the latest window content
independently so a replacement client can build a fresh snapshot without
preserving old transport handles.

## Selection And Diagnostics

The native client selects the network carrier when configured with a remote
endpoint and mutual-TLS identity; otherwise it performs normal local discovery
and negotiation. Automatic local selection offers DMA-BUF followed by shared memory. An
operator-forced local frame transport narrows that capability offer and makes
unsupported selection fail visibly. Exact client arguments are defined by
`ClientArgs` and `FrameTransportArg` in `crates/brood-cli/src/cli.rs`; remote
relay and probe arguments are defined in `crates/brood-remote/src/main.rs`.

Both endpoints report the selected carrier, frame transport, protocol version,
connection generation, resource IDs, frame sequences, byte counts, drops,
backpressure, and rejection reasons. Diagnostics use semantic lane names even
when their physical carriers differ.

## Verification

Acceptance covers the complete profile matrix:

- the normal local client imports DMA-BUF frames under a real nested workload;
- the same client is forced to shared-memory images and renders the same
  workload through CPU upload;
- a certificate-validated network connection sends real input, catalog,
  managed-window scene and pixels, decompresses and validates the frame,
  acknowledges presentation, and disconnects cleanly;
- carrier unit tests reject version, lane, bounds, and frame-metadata errors;
- critical traffic remains deliverable while latest-state and frame traffic
  are saturated.

Transport tests must assert observable client or nested-application behavior,
not only successful serialization or socket connection.

## Related Pages

- [[presentation-client]]
- [[wayland-compositor]]
- [[testing]]
- [[../decisions/0007-presentation-transports-and-client-actors]]
