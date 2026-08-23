---
status: target
type: system
security-boundary: process-and-network
runtime-boundary: process-and-network
wire-format: not-frozen
authority: product
last-reviewed: 2026-08-23
code-links:
  - crates/brood-protocol/src/lib.rs
  - crates/brood-ipc/src/local_protocol.rs
  - crates/brood-ipc/src/local_control.rs
  - crates/brood-ipc/src/backend_message.rs
  - crates/brood-ipc/src/shared_image.rs
  - crates/brood-transport/src/network.rs
  - crates/brood-transport/src/artifact.rs
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
- `InteractiveSession` shards carry ordered session-actor traffic. One
  session's large mailbox message cannot block unrelated interactive sessions.
- `InteractiveDaemon` carries daemon-actor traffic independently from session
  interaction.
- `Reliable` carries ordered catalog and resource transitions.
- `LatestState` carries supersedable pointer, viewport, resize, and scene state.
- `Bulk` carries bounded content-addressed artifact exchange.
- `FrameResource` carries frame content independently from scene publication.

Independent flow control is required. A large or stalled frame may reduce
visual freshness, but it must not delay a key transition, disconnect, or
resource-lifecycle message. Every payload is bounded before allocation or
decode. Carrier scheduling gives critical first priority, then interactive
actor streams, then reliable inventory, ahead of latest-state, bulk, and
frame-resource streams. Separate streams without explicit scheduler priority
are not sufficient under congestion.

Managed pane resize records carry the presentation client's resize epoch in
addition to the requested content size. The server associates that epoch with
the applicable Wayland configure serial and returns it only with the first
suitable committed frame. During an active expectation, an untagged frame
cannot replace the visible pane. The client drops each tagged frame that does
not have an exact active expectation. This includes a frame for a retired
epoch. Ordinary refresh resumes only when a later untagged frame arrives after
terminal cleanup.

Protocol version 22 defines this correlation on local and network carriers. A
tagged request contains the backend pane, epoch, exact target pixel extent,
logical viewport, and `scale_120`. The connection generation supplies route
authority; a message cannot name another generation. A local codec rejects an
unsupported version before dispatch. The network encoding carries the same
fields and bounds.

The server issues one distinct XDG configure serial for an accepted tagged
request, including a same-logical-size request. `Configured` reports the exact
logical viewport, pixel target, and scale tuple. Configure acknowledgement is
not content readiness. A suitable commit must use the acknowledged current
serial and a new buffer, must have the exact logical viewport, and must provide
effective source samples greater than or equal to the target pixel extent. The
server captures that commit into an immutable snapshot with the exact target
pixel extent. Each pending tagged snapshot is bound to its pane, epoch, commit,
scene sequence, and distinct transport resource. A later surface commit cannot
replace its pixels.

The exported window frame carries the epoch. `Configured` and the tagged frame
may arrive on different lanes and in either order. The frame tag is sufficient
to stage an exact frame; status remains bounded admission and diagnostic
evidence. A rejection cannot follow a committed tagged snapshot. Scene
publication and presentation keep the snapshot resource alive until the client
releases it. Supersession, timeout, abort, pane removal, or disconnect cleans
unpublished snapshots and advances stale-epoch authority.

After bounded recovery, the client sends one untagged current-size request. The
server retires pending tagged configure authority but does not publish the
already-current surface as recovery. It waits for a later ordinary commit from
that pane. This preserves one complete old or placeholder waited set before
ordinary frame updates resume. An in-flight tagged scene completes under its
existing ownership rules.

Connection establishment compares immutable endpoint capabilities and selects
one profile for the connection generation. Failure to agree on protocol
version, carrier, frame transport, or bounds fails the connection; it does not
silently reinterpret messages. Reconnection creates a new generation and
invalidates every old lease, resource, mapping, and in-flight operation.

## Local Carrier

An internal, mode-`0600` Unix `SOCK_SEQPACKET` endpoint discovers the local
session server, validates the protocol version, and brokers sealed memory,
event, DMA-BUF, and shared-image descriptors with `SCM_RIGHTS`. D-Bus is not a
presentation carrier or bootstrap path. It remains a public control API for
external applications.

Steady-state messages use bounded shared-memory records with eventfd
notifications. Critical input, latest state, actor messages, catalogs, frames,
and correlated backend request/response traffic have independent mappings.
Local records are codecs for `brood-protocol` messages rather than a second
protocol definition. Presentation steady-state control and session operations
never fall back to the packet broker or to D-Bus.

Descriptor values never appear as raw integers in shared memory. The private
broker transfers a generation-scoped resource descriptor after its stable
resource ID appears in a frame record. Generation checks, filesystem
permissions, owner loss, and mapping validation reject stale or
unrelated access.

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
blocking is contained to that lane. Actor traffic is additionally isolated by
daemon realm and a fixed set of session shards while preserving order within
each session mailbox. Every interactive stream owns its sender and receiver
task; a task blocked writing one stream must not become application-level
head-of-line blocking for the others. A handshake exchanges capabilities,
selects the remote profile, and assigns the server's connection generation
before ordinary traffic begins. Transport keepalives preserve an otherwise
idle static desktop. Stream readers remain owned by dedicated receive tasks so
selecting between lanes cannot cancel a partial framed read.

Critical input and latest-state input have separate producers, queues, sender
tasks, network streams, receiver tasks, and local handoff state. Latest-state
publication replaces the previous unsent value; it can never occupy a FIFO in
front of a key or lifecycle transition. The relay's local IPC writer chooses
critical work first whenever both classes are ready. Relay egress queues are
bounded, and every transport task is supervised so an unexpected sender,
receiver, or local bridge exit terminates the connection instead of silently
removing one semantic lane.

Network peers exchange semantic messages with length-delimited bounded
encoding. The baseline frame-resource packet is a losslessly compressed full
window image with explicit pane, resource, sequence, extent, stride, pixel
format, and color space. It establishes correctness and portability. Future
damage, video, or GPU-aware codecs may replace that payload after negotiation;
they must preserve scene/resource lifecycle and must not introduce a second
presentation model. Each lane reuses its bounded wire buffers between messages,
and frame codec work runs outside the network I/O scheduler. The relay retains
at most one complete not-yet-sent scene, so a slow link converges on current
pixels instead of accumulating an ever-older visual queue. The frame forwarder
drains each available IPC burst before encoding and compresses only its newest
complete scene. Its bounded
network-to-backend handoff applies transport backpressure and backs off when the
local shared-memory lane is full instead of growing an unbounded queue or
spinning a runtime worker. It may finish a scene already being written, but
superseded queued scenes do not consume unbounded memory or tail latency.

A relay may adapt a local session-server connection into the network carrier.
The relay is a transport endpoint: it neither computes layout nor hosts
presentation actors. It may verify and stage an authorized session-realm
artifact before asking the local backend to instantiate it. The native remote
client remains responsible for client actors, input policy, final composition,
and acknowledgements. Catalog traffic retains reliable ordering and each actor
mailbox retains order within its realm/session shard. Client backend and
session operations use correlated request/response messages on the reliable
lane, so a remote presentation never reaches back to the server's local DBus
address.

Either endpoint may offer a content-addressed WASM artifact on the independent
bulk lane. Destination realm, allowlist, digest verification, caching, and
runtime authority are defined by [[artifact-distribution]].

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

Scene/frame joins have explicit packet and byte budgets. Late frames older than
the last published sequence are discarded, and the newest complete scene
supersedes older incomplete work. Once a selected scene stops referring to a
network frame resource, the client releases it and both renderer and relay drop
their generation-scoped mappings. The server sends a disconnect acknowledgement
only after the disconnect record has been published and notified on the local
IPC lane. Setup, graceful shutdown, cancellation, and worker joins all have
finite deadlines; dynamic remote attachment performs connection setup outside
the presentation event loop.

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

Managed-resize diagnostics correlate pane, epoch, configure serial, logical and
pixel size, scale, surface commit, scene sequence, transport resource, client
route generation, staging, promotion, recovery, and rejection.

## Verification

Acceptance covers the complete profile matrix:

- the normal local client imports DMA-BUF frames under a real nested workload;
- the same client is forced to shared-memory images and renders the same
  workload through CPU upload;
- a certificate-validated network connection sends real input, catalog,
  managed-window scene and pixels, decompresses and validates the frame,
  acknowledges presentation, and disconnects cleanly;
- carrier unit tests reject version, lane, bounds, and frame-metadata errors;
- a real WASM artifact transfers in each direction and is accepted only by its
  destination realm;
- critical traffic remains deliverable while latest-state and frame traffic
  are saturated.

Transport tests must assert observable client or nested-application behavior,
not only successful serialization or socket connection.

## Related Pages

- [[presentation-client]]
- [[artifact-distribution]]
- [[../../systems/wayland-compositor]]
- [[../../systems/testing]]
- [[../../decisions/0007-presentation-transports-and-client-actors]]
