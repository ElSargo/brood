---
status: target
type: system
security-boundary: mixed
runtime-boundary: process-and-network
wire-format: not-frozen
authority: product
last-reviewed: 2026-07-20
---

# Presentation Client Architecture

## Purpose

Manifold separates the durable session backend from the software that presents
and interacts with that session. A **presentation client** is an independent
viewport over one session. It owns its own layout, interaction state, client
actors, GPU resources, and final rendering. The backend owns session identity,
running managed applications, hosted Wayland protocol state, backend actors,
and access to backend resources.

The normal case is zero or one attached presentation client. A session continues
running with no client. The design permits additional read-only viewers, but it
does not optimize for simultaneous collaborative control.

This boundary exists to provide all of the following without separate product
models:

- local presentation using a DBus bootstrap, shared-memory steady-state
  protocol lanes, and DMA-BUF surface transport where supported;
- remote presentation using an authenticated network carrier and encoded or
  damage-based per-window surface transport;
- one Rust presentation implementation for Linux, macOS, Windows, and the web;
- client-side WASM applications, key policy, layout policy, component UI, and
  graphics that do not cross the network on their interactive path;
- durable backend applications and actors that survive presentation detach;
- explicit, asynchronous access from client actors to backend files, processes,
  services, and session actors.

This document specifies the target behavior and implementation boundaries. It
does not freeze a byte-level network protocol, plugin package format, or render
packet opcode table.

## Goals

1. The backend remains useful and coherent with no attached client.
2. Attaching, detaching, crashing, or replacing a client does not implicitly
   terminate a session or its managed applications.
3. Each client has independent presentation state. There is no canonical
   backend-owned pane layout.
4. One optional presentation lease holder controls hosted Wayland sizing and
   input for a session.
5. Clients request frame streams only for managed windows they currently need.
6. DMA-BUF is a preferred local frame transport, not the pane or frame model.
7. WGPU is the only presentation renderer. Platform-specific import, window,
   input, accessibility, and media adapters may exist around it.
8. Presentation actors run locally and can handle input, layout, UI, and custom
   graphics without a backend round trip.
9. Backend operations remain asynchronous and capability-rooted from a client
   actor's perspective.
10. The protocol is bounded, generation-safe, observable, and recoverable after
    either endpoint disappears.

## Non-Goals

The first implementation does not provide:

- concurrent collaborative editing of one viewport layout;
- multiple clients independently resizing one hosted Wayland toplevel;
- transparent live migration of a running WASM actor between client and backend;
- exactly-once behavior across arbitrary network disconnects;
- a universal zero-copy mechanism across every platform;
- a backend-composed remote desktop framebuffer;
- native OS widgets as the plugin ABI;
- automatic execution of untrusted plugin code supplied by a remote backend;
- peer-to-peer presentation clients;
- preservation of endpoint-local GPU resources after the client process exits.

## Terms

**Session backend**
: The authoritative runtime for a session. It owns managed applications,
  hosted Wayland state, backend actors, session resources, and the presentation
  protocol service. The current implementation keeps these roles in
  `manifoldd`.

**Presentation client**
: A native or web program that opens one or more presentation surfaces and runs
  the endpoint-local actor and rendering runtime. The planned native binary is
  referred to as `manifold-client` in this document, but that executable name
  is not an ABI commitment.

**Viewport**
: One client's local presentation of one session, including local layout,
  visibility, focus, animation, and overlays. Viewport state is not backend
  session state.

**Presentation lease**
: A generation-bearing authority granted to one client connection for one
  session. It authorizes hosted-window configuration and primary application
  input. Other clients may observe without holding the lease.

**Window producer**
: A backend-managed application window and its committed surface content. A
  producer is discoverable independently of whether any client subscribes to
  its frames.

**Frame subscription**
: A client request for current and future content from a window producer using
  a negotiated transport.

**Presentation actor**
: A plugin actor instantiated in the presentation client for one viewport. It
  may own endpoint-local application, policy, component, or graphics state.

**Session actor**
: A plugin actor instantiated in the backend. It may own durable or shared
  session behavior and access backend capabilities.

## Required Invariants

- Session identity, managed-application identity, and backend actor state do not
  depend on an attached client.
- Viewport layout, visibility, hover, animation, and local chrome do not become
  backend-owned merely because they may be persisted there on request.
- Closing or hiding a client pane never implicitly closes its backend window
  producer.
- Only the current lease generation can configure or send primary input to
  hosted Wayland applications.
- A stale connection, stale entity generation, stale resource handle, or stale
  lease generation cannot acquire authority over a replacement object.
- The backend never waits synchronously for client policy or rendering in order
  to service unrelated session work.
- The client never waits synchronously on network or backend I/O while handling
  immediate input or rendering.
- Frame and render-update queues are bounded and may supersede intermediate
  latest-state updates.
- Reliable control state and supersedable frame state use distinct flow-control
  rules.
- A frame transport owns complete acquire, reuse, cancellation, disconnect, and
  release semantics.
- WGPU and WGSL are implementation choices of the presentation runtime, not
  host object types carried in the cross-process or plugin ABI.

## System Topology

```text
Session backend
  session and window registry
  hosted Wayland compositor
  backend/session WASM actors
  filesystem/process/network services
  surface snapshot and frame broker
  lease and input authority
  presentation protocol endpoint
             |
             | local:  DBus bootstrap -> shared-memory protocol lanes
             |         + DMA-BUF or copied per-window surfaces
             |
             | remote: authenticated network carrier
             |         + encoded or damage-based per-window surfaces
             |
Presentation client
  connection/session mirrors
  viewport, layout, focus, hit testing, and presentation scene
  presentation WASM actors and policy lanes
  local actor mailboxes and client persistence
  local component, text, and graphics services
  WGPU compositor
  native or web platform shell
```

Presentation actors execute in the client in both local and remote deployments.
Their local messages, policy decisions, UI updates, animations, and render
submissions do not cross the presentation connection. Only addressed session
actor messages and asynchronous backend capability operations cross to
`manifoldd`.

A local deployment may initially host both halves in one process. It must still
use the same typed boundary and ownership rules. Direct references from client
presentation code into mutable backend state are transitional defects, not an
optimization to preserve.

## Ownership

| Concern | Backend | Presentation client |
| --- | --- | --- |
| Session identity and lifecycle | authoritative | mirrored |
| Running managed applications | authoritative | observed/commanded |
| Hosted Wayland protocol objects | authoritative | never exposed directly |
| Window producer metadata | authoritative | revisioned mirror |
| Viewport layout and visibility | optional persisted preference only | authoritative |
| Final focus and hit testing | validates application input authority | authoritative for viewport |
| Wayland configured content size | applies configure under lease | requested by lease holder |
| Native chrome and overlays | no | authoritative |
| Client actor transient state | no | authoritative |
| Client-local persistence | no | authoritative |
| Session/backend actor state | authoritative | messages/snapshots only |
| Backend files and processes | authoritative capability service | asynchronous requester |
| GPU device and swapchain | no | authoritative WGPU owner |
| Client render packets/resources | no by default | authoritative |
| Hosted-window frame production | authoritative | subscribed consumer |
| Frame decoding/import | transport producer/converter as needed | transport consumer |
| Accessibility/IME/clipboard UI | mediates session operations as needed | platform-facing owner |

## State Scopes

### Viewport-ephemeral state

This state normally disappears when a viewport closes:

- hover and pressed states;
- in-progress gestures and animations;
- transient menus, tooltips, and drag previews;
- frame caches and GPU resources;
- temporary query and selection state.

### Client-persisted state

A presentation actor may persist state on the client, keyed at least by backend
identity, session identity, plugin identity, actor role, and schema version.
Examples include local layout, preferred zoom, panel visibility, and editor UI
preferences. Client persistence does not imply that another client can see the
state.

### Session-coordinated state

A presentation actor may explicitly send state to a session actor or use a
backend storage capability. This is appropriate for shared application models,
resumable tasks, session-specific documents, and preferences intended to follow
the session. Coordination is opt-in; the runtime does not continuously merge
client and backend heaps.

### Graphics state

WGPU devices, pipelines, bind groups, textures, component caches, and accepted
client render packets are endpoint-local. They are rebuilt after client restart
or unrecoverable device loss. An application that requires durable reconstruction
stores its model or source assets in client persistence, a session actor, or a
backend capability rather than treating GPU state as durable.

## Identity And Generation Model

Stable logical identifiers and replaceable runtime handles are separate.
Protocol entities use opaque identifiers plus generations where reuse or
replacement is possible.

Required identities include:

- `BackendId`: stable identity used to namespace client persistence;
- `SessionId`: stable backend session identity;
- `ConnectionId`: one authenticated protocol connection;
- `EndpointId`: one presentation-client installation or profile;
- `ViewportId`: one endpoint-local view of a session;
- `PaneId`: stable presentation unit where the backend owns such a unit;
- `WindowId`: one managed toplevel producer;
- `SurfaceNodeId`: one node in a committed surface tree;
- `ActorAddress`: package, actor role, scope, and instance identity;
- `LeaseGeneration`: monotonically increasing authority generation;
- `InventoryRevision`: backend window/session registry revision;
- `FrameSequence`: producer-local committed frame sequence;
- `SubscriptionId`: connection-generation-scoped frame subscription;
- `OperationId`: connection or actor-generation-scoped asynchronous operation.

An identifier received from one backend or connection must not be accepted in
another namespace. Wire encodings must not expose pointers, Smithay object IDs,
WGPU handles, file descriptors without transport framing, or Rust memory layout.

## Connection Lifecycle

A presentation connection progresses through:

```text
connecting
  -> authenticated
  -> protocol-negotiated
  -> inventory-synchronized
  -> active
  -> draining or disconnected
```

The local handshake uses DBus for discovery, activation, authentication, and
exchange of connection-generation resources. It establishes size-sealed
shared-memory regions and event notifications for the steady-state protocol. DBus remains a
cold descriptor broker for resources such as DMA-BUF pools; it is not on the
per-input or per-frame message path. A dedicated Unix-socket message transport
is not part of the target local profile.

A remote handshake uses an authenticated encrypted network carrier. QUIC is the
preferred starting point because it supplies secure multiplexed streams,
datagrams, congestion control, and connection liveness; the carrier remains
replaceable behind the protocol lane abstraction.

Every handshake establishes:

- protocol major and minor versions;
- backend and endpoint identity;
- authentication and session access;
- supported control features;
- supported frame transport families;
- maximum message, resource, and in-flight frame budgets;
- compression and checksum support where applicable;
- client WGPU baseline and optional Manifold graphics features;
- installed or approved plugin package identities and versions.

A major-version mismatch fails before session mutation. Minor and feature
negotiation uses explicit feature identifiers. Unknown mandatory records fail
the containing operation; unknown optional length-tagged records may be skipped.

The backend sends a complete inventory snapshot before deltas. Each snapshot
has an `InventoryRevision`; subsequent deltas apply only to the immediately
expected revision. A gap causes resynchronization rather than speculative patch
application.

Reconnect creates a new connection generation. Client-local viewport state may
be reused, but leases, subscriptions, operation handles, imported buffers, and
in-flight input serials from the old connection are invalid.

## Session And Window Inventory

Inventory is cheap control state and is delivered independently of pixel
streams. It includes enough information to render switchers, tabs, placeholders,
and recovery UI without subscribing to every window.

A window inventory record includes at least:

- stable `WindowId` and entity generation;
- owning `SessionId` and managed application identity;
- title, application ID, role, and lifecycle state;
- parent/transient relationship;
- current committed logical content extent;
- minimum and maximum extent where known;
- aspect and resize constraints where known;
- current buffer scale and transform metadata;
- attention, urgency, activation, and close capability state;
- current frame sequence and whether content is available;
- producer failure or recovery information;
- supported subscription classes.

Metadata strings and aggregate records are bounded. Frequent fields may use
patches, but patches retain the inventory revision discipline.

## Presentation Lease

### Scope

The initial implementation has at most one `PresentationLease` per session.
The lease holder has configure and primary input authority for all hosted
windows in that session. This is intentionally simpler than per-window lease
negotiation while preserving per-window validation internally.

The formal protocol includes the lease generation in every authoritative window
operation, so a future implementation may narrow authority without changing the
stale-command rule.

### Acquisition

The first eligible client viewing a session normally requests and receives the
lease automatically. A grant contains:

```text
session
holder endpoint and connection
lease generation
grant inventory revision
optional expiry or liveness policy
```

The backend is the sole lease arbiter. There is no distributed election.
Additional clients may subscribe as spectators but cannot configure windows or
send hosted-application input.

### Release And Revocation

A lease ends through explicit release, session closure, endpoint replacement,
administrative revocation, or connection loss. The backend may use a bounded
reconnect grace period, but stale-generation commands are rejected as soon as a
new generation is granted.

Revocation does not close applications or remove windows. Their last accepted
configured sizes remain in force until a new holder requests changes.

### Authority Boundaries

The lease authorizes:

- desired Wayland toplevel content size;
- activation and primary hosted-surface focus;
- keyboard, pointer, button, axis, and touch input when supported;
- interactive move/resize protocol policy;
- popup positioning constraints that depend on the viewport.

The lease does not authorize:

- process termination without the corresponding backend capability;
- mutation of backend actor state except through actor/service APIs;
- another client's layout or local state;
- another client's subscription choices;
- bypass of session capability validation.

## Independent Viewports

A client may arrange, hide, scale, animate, or decorate subscribed windows
without changing backend session state. Presentation actions and backend actions
are separate commands:

- hiding a pane is local and normally suspends or drops its frame subscription;
- closing a viewport pane is local;
- requesting a managed application to close is an explicit backend operation;
- forcibly terminating a process is a distinct capability-protected operation.

Presentation actors may publish optional placement and attention requests. The
viewport may honor, adapt, or reject them. A saved layout is either client-local
state or an explicit persisted preference; it is never inferred from the current
backend surface geometry.

## Hosted Wayland Window Model

### Window producers

The backend maps each hosted xdg toplevel to a `WindowId`. A committed window
frame is a snapshot of the relevant Wayland surface tree, not merely the root
`wl_buffer`. It captures:

- root and synchronized subsurface ordering;
- node-relative positions and transforms;
- crop, viewport, and buffer scale;
- damage accumulated for the committed snapshot;
- opaque and input regions where relevant;
- buffer references and acquire synchronization;
- logical content extent and frame sequence.

Smithay, `wl_surface`, `wl_buffer`, EGL, and renderer objects remain private to
the backend. The broker converts committed state into transport-neutral producer
state and transport-specific frame offers.

### Popups And Transients

XDG popups and transient toplevel relationships are represented explicitly.
The backend retains protocol authority and configure/ack state; the client
places their presented content relative to the parent according to backend
geometry and viewport constraints supplied by the lease holder. The initial
implementation may compose a synchronized popup/subsurface tree into one
transport frame if independent transport would violate atomicity, but metadata
must preserve role and input geometry.

### Atomic commits

A client must not combine nodes from incompatible surface-tree commits. A frame
offer identifies one atomic producer snapshot. Transport implementations may
send unchanged nodes by reference or damage, but the resulting client frame is
committed as one sequence.

## Window Configuration

The lease holder requests a logical **content extent**. Client-side decorations,
borders, tabs, and shadows are excluded. A configure request contains:

```text
session
window and generation
lease generation
client request sequence
desired logical width and height
interactive or settled intent
optional preferred presentation scale
```

The backend validates the lease, window constraints, arithmetic, and request
ordering before issuing the appropriate Wayland configure. It reports the
accepted requested extent and later reports the extent actually committed by
the application. The client continues displaying the previous frame, scaled or
letterboxed according to local policy, while waiting for the new commit.

Interactive resize is latest-state traffic. Intermediate requests may be
coalesced before Wayland configure as long as protocol serial correctness is
preserved. A settled request must eventually receive an accepted, rejected, or
superseded result.

Clients without the lease scale, crop, or letterbox the controller-selected
content. The backend does not reconfigure one application separately for each
viewer.

## Frame Subscription

### Discovery and request

A client learns about windows through inventory, then requests content only for
windows it needs. A subscription request includes:

- window identity and generation;
- desired current extent and scale information;
- visibility and latency intent;
- supported transport families and transport-specific capabilities;
- desired quality class;
- decode/import limits;
- whether a cached initial frame is acceptable.

The extent is a transport preference unless the requester also holds and uses
the presentation lease. Subscribing alone never resizes an application.

### Subscription lifecycle

```text
requested
  -> negotiated
  -> active
  -> suspended
  -> active
  -> draining
  -> closed
```

Suspension stops new frame production or delivery while allowing the client to
retain its last imported texture. Closing releases all connection-scoped frame
leases and transport resources. The backend may close a subscription when its
window disappears, transport fails without fallback, budgets are exceeded, or
the connection ends.

### Latest-state behavior

Each subscription has a small bounded number of offered and acquired frames.
When the client falls behind, the broker drops superseded frames and sends the
newest reconstructible state. Damage-only transports must retain or regenerate a
key state before dropping dependencies.

The backend aggregates active subscriptions. With no subscribers it avoids
conversion, encoding, and transport work. It may suppress Wayland frame
callbacks according to compositor policy so invisible applications do not
render continuously.

## Transport Architecture

The connection carrier and surface transport are negotiated separately.
Ordinary protocol records use semantic reliable, critical, latest-state, and
bulk lanes. Surface subscriptions select a frame transport appropriate to the
endpoints and producer. A local connection therefore uses shared memory for
ordinary traffic while concurrently referencing DMA-BUF or copied-image frame
resources; a remote connection maps the same protocol semantics onto a network
carrier and selects an encoded or damage-based frame transport.

A frame is a producer snapshot; a frame transport is how that snapshot reaches
one client. Transport negotiation selects a concrete family rather than filling
a single union with every platform's fields.

Every transport must define:

- capability negotiation;
- frame metadata and payload framing;
- acquire synchronization;
- damage and dependency semantics;
- maximum dimensions and bytes;
- client import/decode completion;
- backend and Wayland buffer release point;
- supersession and cancellation;
- disconnect cleanup;
- corruption and unsupported-format fallback;
- diagnostics and timing.

### Color ownership

The backend preserves each managed-window snapshot's pixel encoding and carries
its color-space, alpha, and format metadata with the frame resource. It does not
convert buffers for the display attached to a presentation client, because the
backend may be headless, remote, or observed by clients on different displays.

The presentation client imports or decodes the source encoding into its linear
composition space. After combining managed windows, client UI, and overlays, it
converts to the color space selected for the current operating-system surface.
The platform shell reevaluates that surface when the window moves between
displays. The operating system remains responsible for the final transform from
the declared surface color space through the current monitor profile.

An untagged SDR Wayland producer is interpreted as sRGB. Wide-gamut and HDR
producers require explicit frame metadata; neither endpoint infers those
properties from a DRM fourcc or an eight- or ten-bit channel layout.

### Required initial transport: copied image

The first end-to-end transport uses a bounded copied image representation, with
an explicitly negotiated pixel format, row stride, extent, color space, alpha
mode, and damage. It is not the preferred final local path, but it proves the
process boundary, frame lifecycle, WGPU upload, fallback behavior, and tests
without relying on external-memory support.

### Local DMA-BUF transport

The Linux fast path registers reusable DMA-BUF pools through the DBus descriptor
broker. Registration transfers plane descriptors, format, modifier, extent,
color metadata, and synchronization capabilities and returns a
generation-scoped resource ID. Only after registration is acknowledged may
shared-memory frame records reference that resource ID. Per-frame descriptor
transfer is not the hot path.

Frame offers and releases travel through the shared-memory lanes. Reusable DRM
synchronization objects or equivalent timelines are registered as resources;
frame records carry timeline points rather than transferring a new fence FD per
frame. Where an upstream producer supplies a one-shot fence, the backend imports
it into the registered synchronization timeline before publishing the frame.
Acquire and release synchronization must be explicit where supported, and
resource IDs, frame sequences, and connection generations prevent stale
mappings from being used after reconnect or pool replacement. The client imports
the image through a Linux-specific WGPU integration adapter.

WGPU remains the only renderer even if the importer uses WGPU HAL or platform
APIs. If direct import, modifier support, or synchronization is unavailable,
the endpoints renegotiate or the backend converts to copied-image transport.
DMA-BUF support is never assumed from operating-system identity alone.

A directly forwarded Wayland buffer is not released to its producer until all
uses requiring its immutable contents have completed. The broker may instead
copy/import into backend-owned storage and release earlier. The chosen strategy
must be visible in diagnostics and bounded by retained-frame budgets.

### Remote transports

Remote implementations preserve per-window subscriptions and client-side
composition. The initial network carrier should be evaluated as QUIC: reliable
streams carry control and reconstructible bulk state, while datagrams may carry
bounded supersedable records. Raw UDP is not a protocol baseline because doing
so would require Manifold to recreate authentication, encryption, congestion,
retransmission, multiplexing, and path handling.

Remote frame transports may add:

- lossless damage tiles;
- compressed still frames;
- encoded video streams;
- platform/browser external-video frames.

These transports preserve per-window identity and client-side composition. A
single backend-composed desktop stream is not the default model, though a later
explicit compatibility mode may provide one.

Encoded and damage-dependent transports define keyframe recovery, decoder reset,
quality adaptation, and congestion behavior. Media decode is a platform service
feeding WGPU textures; it is not a second presentation renderer.

## Frame Lifetime

A logical frame lease progresses through:

```text
produced
  -> offered
  -> acquired or superseded
  -> imported/decoded
  -> presented or superseded
  -> released
```

The client acknowledges import or decode separately from optional presentation
feedback when the transport needs that distinction. Backend resource release
must not depend indefinitely on presentation feedback. Timeouts or disconnect
force connection-resource cleanup while respecting any platform operation that
cannot be cancelled safely.

The client may retain its own WGPU texture after releasing the transport frame.
The release says that transport-owned source resources may be reused; it does
not require removing the visual from the viewport.

## Input Routing

The presentation client performs final local hit testing over its scene. Input
first passes through client-local policy and application actors. If not consumed
and the target is a hosted window, the client sends a bounded input record to
the backend.

A hosted-window input record includes:

- session and window identity;
- lease generation;
- endpoint-local device and event sequence;
- event timestamp in a negotiated monotonic domain or mapped timestamp;
- input kind and bounded payload;
- surface-local logical coordinates where applicable;
- frame/configuration sequence used for coordinate mapping;
- modifier and button state required to recover after dropped motion events.

Only the lease holder's hosted-application input is accepted. Pointer motion and
scroll may be coalesced; key transitions, button transitions, focus transitions,
and protocol-significant events remain ordered. On reconnect, lease change, or
input-stream gap, the backend resets pressed-state assumptions before accepting
a new synchronized input state.

The backend maps accepted input into its Wayland seat. The initial product may
use one logical primary seat because zero or one controlling client is the target.
Additional viewers do not receive independent application seats.

IME, clipboard, drag-and-drop, cursor, relative-pointer, pointer-lock, tablet,
and touch behavior are separate negotiated input/platform features. Their
protocols must preserve the same lease and generation checks.

## Client-Side WASM Runtime

### Placement roles

A plugin package may declare actors and policy lanes with explicit placement:

- `presentation`: instantiated in a presentation client for a viewport;
- `session`: instantiated in the session backend;
- `either`: eligible for either runtime only when its required capabilities and
  lifecycle contract are satisfied.

The first implementation does not migrate an actor after instantiation. Packages
that need both locations expose separate actors and communicate by mailbox or a
typed service protocol.

### Presentation actor scope

A presentation actor instance is scoped to an endpoint, backend, session,
viewport, package, actor role, and actor generation. It starts after inventory
synchronization and before the viewport is considered fully ready. It receives
an initial bounded snapshot rather than requiring replay of the entire session
event history.

It may:

- handle local semantic input events;
- maintain transient or client-persisted state;
- create local component or custom-graphics panes;
- register local actions and key policy;
- propose or compute viewport layout;
- message local presentation actors without network traffic;
- address session actors through the connection;
- submit asynchronous backend capability operations;
- rebuild local graphics after renderer recovery.

It cannot directly mutate backend session state, access backend files, configure
hosted windows, or send hosted input without the corresponding service and
lease checks.

### Local hot paths

The following paths remain entirely within the client when their participants
are local:

```text
key or pointer -> local policy -> local actor -> component/render update
window inventory event -> local layout actor -> local viewport commit
local actor -> local actor mailbox
render packet -> local validation -> local WGPU lowering
```

Network completions and remote actor messages enter bounded reserved actor lanes.
They never run a network wait inside synchronous key or layout policy.

### Cross-runtime actor messages

Actor addresses include scope and placement. The local registry resolves local
recipients directly; the connection router forwards session-scoped recipients.
Remote delivery is bounded and ordered per sender/recipient stream while a
connection remains active. Reconnect does not promise transparent exactly-once
delivery. Applications needing durable commands use operation IDs, backend
persistence, and explicit acknowledgement.

### Plugin distribution and trust

A backend may advertise required or available package identities and
content hashes. A client runs only packages allowed by its installation and
connection trust policy. Receiving a WASM artifact from a remote backend does
not itself authorize execution. Package signing, installation UX, and federation
policy require a separate specification before arbitrary remote package delivery
is enabled.

WASM isolation remains valuable, but GPU, memory, CPU, message, and resource
limits protect endpoint coherence rather than claiming that approved plugin code
is harmless.

## Client Applications

A real Manifold application may be primarily client-side. Common shapes are:

### Client-local application

One presentation actor owns the application model and UI. It may persist locally
and need no backend actor. It exists independently on each client.

### Client application with backend storage

The presentation actor owns interaction and rendering while using backend file,
database, or object-storage operations for selected durable state.

### Paired presentation and session actors

A presentation actor owns endpoint-local UI and a session actor owns durable or
shared application state. They exchange bounded messages and state snapshots.
Neither actor shares mutable memory with the other.

### Managed native application

The backend owns the external process and Wayland producer. The client consumes
window frames and sends input under the lease. Client actors may add chrome,
commands, or companion panes without becoming the native application's state
owner.

## Backend Capabilities From The Client

Backend files, processes, sockets, watches, credentials, and services are exposed
as asynchronous capability-rooted operations. They follow the same completion
and generation principles as backend-local actor I/O:

```text
presentation actor -> local host import -> connection operation
backend service -> result or failure -> reserved local completion lane
```

Client APIs must not assume that backend paths are meaningful on the client.
Large streams use bounded chunking and flow control rather than mailbox-sized
messages. Cancellation reports whether cancellation won the operation race; it
does not retroactively undo external side effects.

A remote presentation connection is a stronger boundary than communication
between trusted plugins inside one local session. Authentication and per-session
service authority are enforced by the backend regardless of the client's local
plugin trust policy.

## WGPU Presentation Runtime

### Renderer

The presentation client has one WGPU-based renderer and scene compositor. Native
builds use the WGPU backends available on the platform; web builds use WebGPU.
The client does not maintain a parallel GLES, Vulkan, Metal, or Direct3D scene
renderer.

The renderer composites:

- imported or uploaded managed-window textures;
- presentation-rendered component trees;
- plugin render packets and canvas scenes;
- text and image resources;
- client chrome, menus, cursors, overlays, and recovery surfaces.

Platform-specific APIs may supply external textures or decoded frames, but those
become resources in the WGPU composition graph.

### Portable baseline

Manifold defines a required graphics baseline from the common supported WGPU and
WebGPU behavior. It includes explicit limits and formats rather than inferring
features from backend names. Optional features use Manifold feature identifiers
and require fallback content or a clear unavailable state.

WGSL is the shader source language for custom plugin pipelines. Source is
validated and pipelines are compiled asynchronously relative to actor dispatch.
A plugin cannot obtain WGPU device, queue, command encoder, swapchain, or native
backend objects.

### Platform shell

WGPU does not replace platform integration. Thin native and web adapters own:

- operating-system windows or browser canvases;
- display scale and color information;
- keyboard, pointer, touch, and IME events;
- accessibility tree projection;
- clipboard and drag-and-drop;
- local connection and descriptor transfer;
- web networking and storage APIs;
- media decoding and external-image import.

The shared Rust client core owns viewport semantics, actor execution, scene
construction, transport state machines, and rendering policy.

### Device loss

The client renderer reports device loss to the viewport and presentation actor
runtime. Managed-window subscriptions provide a new current frame as needed.
Component scenes and plugin graphics are reconstructed from accepted CPU-side
state or by actor resubmission. Recovery is bounded; unrecoverable panes display
client-native recovery content without affecting backend session liveness.

## Plugin UI And Graphics

Component trees and custom graphics are presentation-local outputs. A
presentation actor submits them directly to the local client host. The backend
is not on the input-to-frame path and does not normally retain their GPU
resources or render packets.

The presentation client owns:

- component rendering and theme realization;
- text shaping and endpoint font integration;
- accessibility projection;
- graphics packet validation and WGPU lowering;
- resource budgets and GPU synchronization;
- final pane clipping and composition;
- frame pacing, damage, and device recovery.

The actor owns its application model, component description, semantic tree,
shader source, and graphics resource requests. If it wants these to survive
client replacement, it persists reconstructible state locally or coordinates
with the backend.

The render-packet ABI remains Manifold-owned rather than exposing WGPU Rust
objects. Its portable baseline should map directly and predictably to WGPU/WGSL.
See [[plugin-ui]] and [[plugin-graphics]]; those protocols must be finalized only
after the client renderer and actor-placement boundary are proven.

## Control And Data Lanes

The logical protocol separates traffic classes independently of the carrier.
The target mappings are:

| Semantic traffic | Local carrier | Remote carrier |
| --- | --- | --- |
| Critical reliable | Reserved shared-memory ring | Reserved reliable stream capacity |
| Reliable ordered | Shared-memory ring | Reliable multiplexed stream |
| Latest state | Coalescing shared-memory slots/ring | Datagrams or replaceable stream state |
| Bulk | Registered memfd-backed resource | Independently flow-controlled stream |
| Surface frame | DMA-BUF or copied-image resource | Encoded or damage-based per-window transport |
| Wake and liveness | Event notification, DBus owner, optional pidfd | Carrier connection state |

The local carrier uses independent bounded single-producer/single-consumer
regions in each direction. Monotonic sequence positions, acquire/release
atomics, cache-line-separated ownership, validated record lengths, and
connection generations define the shared ABI. Event notifications are batched
and normally emitted only when a lane changes from empty to non-empty. No
cross-process mutex is part of the baseline.

Reliable exhaustion applies explicit backpressure. Latest-state exhaustion
coalesces or supersedes records. Reserved critical capacity ensures that lease
revocation, key/button releases, terminal results, and teardown cannot be
starved by frame or motion traffic.

### Reliable ordered control

Used for:

- handshake and authentication;
- inventory snapshots and deltas;
- lease acquisition and revocation;
- authoritative commands and results;
- actor messages and operation completions;
- subscription setup and transport negotiation;
- resource teardown and diagnostics requests.

Reliable does not mean unbounded. Each lane has byte and item budgets, and
reserved capacity prevents frame churn from starving lease revocation or
teardown.

### Latest-state traffic

Used for:

- frame offers;
- damage and cursor updates;
- interactive resize requests;
- coalescible pointer motion and scroll;
- presentation feedback;
- local plugin render and view revisions.

Intermediate values may be superseded while preserving terminal, transition,
and synchronization records.

### Bulk streams

Used for copied frames, encoded media, file contents, immutable assets, and other
large data. Bulk transfer has independent flow control and cannot allocate its
declared total size before validating limits.

## Backpressure And Budgets

Both endpoints advertise generous but finite limits. At minimum, account for:

- control messages and bytes per lane;
- actor mailbox messages and bytes;
- outstanding backend operations;
- inventory entities and metadata bytes;
- subscriptions per connection and per window;
- offered/acquired frames and retained Wayland buffers;
- copied frame and encoded-frame bytes;
- decoder and imported-texture memory estimates;
- client WASM memories, tables, workers, and CPU fuel/time;
- graphics packets, resources, pipelines, and compile queues.

Exhaustion returns typed backpressure or limit failures. It does not silently
fall back to unbounded queues. Frame paths prefer dropping superseded work;
control paths reject new non-reserved work while preserving teardown and terminal
completions.

## Failure And Recovery

### Client disconnect

The backend:

- revokes or begins grace handling for its lease;
- stops accepting its input and configure commands;
- closes its subscriptions and transport resources;
- cancels connection-scoped operations according to operation semantics;
- retains the session, applications, backend actors, and window inventory;
- continues serving unrelated clients and backend work.

### Backend disconnect

The client:

- marks backend windows disconnected;
- stops sending authoritative operations;
- may retain last imported textures for a bounded recovery display;
- keeps purely client-local actors running when their lifecycle allows;
- fails or suspends backend operation handles explicitly;
- reconnects with a fresh connection generation and inventory snapshot.

It must not replay ambiguous mutating commands automatically unless their API
provides idempotency and backend deduplication.

### Window producer failure

The backend emits a terminal or recovery lifecycle state. The client preserves
its local pane shell if desired and renders recovery UI. A replacement producer
uses a new entity generation even if a stable application-level pane identity is
preserved.

### Transport failure

A subscription may renegotiate to a lower transport without changing
`WindowId`. The client keeps the last valid frame during negotiation. A transport
failure does not transfer presentation authority or terminate the application.

## Security And Trust Boundaries

Local installed plugins inside one session remain trusted peers under the
existing product model. A presentation protocol connection, especially a remote
one, is not automatically such a peer.

The backend validates:

- authentication and session access;
- lease generation and command authority;
- actor addresses and capability grants;
- identifiers, generations, sizes, arithmetic, and message bounds;
- file/process/network operations;
- transport resource ownership and release.

The client validates:

- backend identity according to connection policy;
- inventory and transport encoding;
- external image dimensions, formats, and resource counts;
- plugin package approval and content identity;
- render packets, shaders, and local actor resource use.

Neither side treats a DMA-BUF descriptor, encoded frame, WASM module, shader, or
length field as trusted merely because the peer is local.

## Diagnostics

Backend diagnostics expose:

- active connections, endpoint identities, and negotiated protocol features;
- lease holder, generation, grant/revoke reason, and stale-command counts;
- inventory revision and resynchronization counts;
- per-window subscriber counts and current committed extent;
- transport choice, conversion path, damage, frame sequence, and drop reason;
- retained Wayland buffers, frame leases, encode/copy timing, and bytes;
- rejected input, configure, actor, and capability operations;
- detach, reconnect, and cleanup state.

Client diagnostics expose:

- connection state and inventory mirror revision;
- viewport/session association and lease state;
- visible panes and active subscriptions;
- import, upload, decode, WGPU render, and presentation timing;
- frame age, dropped/superseded sequences, and texture memory;
- local actor latency, mailbox pressure, and policy timing;
- component/render-packet revisions and failures;
- adapter capabilities, optional-feature fallback, and device recovery.

A correlated trace uses connection, session, window, subscription, frame, actor,
and operation identifiers without depending on synchronized wall clocks.

## Current Refactor Baseline

The pre-transition cleanup established boundaries that the presentation split
must extend rather than replace:

- `runtime.rs` owns session startup, calloop driving, initial presentation
  attachment, and ordered shutdown; `compositor.rs` is now focused on Smithay
  protocol wiring, and `nested_test.rs` independently owns test topology and
  supervised-process policy.
- `state_services.rs` gives session catalogue, focus identity, extension
  registry, and test-harness lifecycle explicit fallible owners. `Manifold`
  still coordinates Smithay and presentation effects, but new presentation
  protocol state should be another typed service rather than new parallel fields
  on `Manifold`.
- `geometry.rs` centralizes checked conversion between normalized layout,
  logical pixels, host extents, physical input, capture sizes, and Wayland axis
  values. The split protocol must preserve those policies and move generally
  useful geometry types into a shared boundary instead of recreating unchecked
  casts in the client.
- `winit_presenter.rs`, `debug_dump.rs`, and the Smithay dispatch path now
  confine raw handles and mappings behind audited typed owners. The WGPU shell
  and external-frame import adapters must keep the same narrow unsafe-boundary
  discipline.
- The current SHM debug path proves that a late DBus request cannot recover a
  transient `BufferAssignment` after Smithay consumes it. The frame broker must
  therefore observe or retain committed content at the Wayland commit/renderer
  boundary; debug capture cannot be repurposed as the frame source.
- Core-module replacement now prepares an unreachable candidate generation,
  validates it, stops and joins the old generation, and atomically publishes the
  replacement. Component and Preview 1 actors have typed start/stop hooks,
  generation-owned resources, and structured rebind/shutdown.
- `resource_limits.rs` provides aggregate Wasmtime memory/table limits and
  generation byte budgets. Completion, mailbox, worker, watch, process, PTY,
  socket, timer, and stream resources are bounded and generation-owned. The
  presentation actor host and connection router must reuse these ownership
  rules rather than introducing a second unbounded runtime.
- Typed key-policy invocation keeps event context and immutable publications
  together. Moving the policy host to the client should preserve that typed
  invocation and only change placement and routing.

These changes reduce the first phase from a general central-state cleanup to a
specific extraction of presentation authority, mirrors, and frame ownership.
They do not alter the target backend/client ownership model.

## Implementation Structure

The target Rust workspace should separate responsibilities into libraries before
or while introducing a second process. Exact crate names may change, but the
boundaries are required:

- **protocol model**: versioned identities, records, state machines, validation,
  and transport-independent serialization;
- **backend presentation service**: inventory, leases, subscriptions, input
  validation, actor routing, and capability routing;
- **Wayland frame broker**: committed surface snapshots, buffer retention,
  conversion, damage, and transport production;
- **client connection mirror**: handshake, snapshots/deltas, reconnect, command
  results, and backpressure;
- **client viewport model**: local scene, layout, focus, subscriptions, and
  presentation requests;
- **client WASM host**: presentation actors, policy lanes, persistence, and
  local/remote routing;
- **WGPU renderer**: scene lowering, component and plugin graphics, managed-frame
  textures, composition, and recovery;
- **platform shell**: native Winit and web adapters;
- **local connection carrier**: DBus bootstrap and descriptor brokering plus
  bounded bidirectional shared-memory lanes and event notifications;
- **remote connection carrier**: authenticated encrypted streams and
  supersedable delivery, initially targeting QUIC semantics;
- **surface transports**: copied image, local DMA-BUF, and remote encoded or
  damage-based per-window streams.

The first backend presentation service may live inside `manifoldd`, but it must
follow the transition/effect pattern established by `state_services.rs`: validate
and commit inventory, lease, and subscription transitions in the service, then
apply Smithay effects from the compositor owner. Startup and shutdown integration
belongs in `runtime.rs`; automated topology remains in `nested_test.rs`.

`manifoldd` must no longer expose mutable `Manifold` state to presentation code.
The current GLES presenter may remain as a temporary compatibility implementation
but is not part of the target client architecture.

## Implementation Plan

### Phase 0: executable protocol model

- Create typed IDs, generations, inventory records, lease state, subscription
  state, frame sequences, and errors.
- Extract shared logical geometry and checked coordinate/extent policies from
  the current `geometry.rs` boundary without making Smithay or Winit types part
  of the protocol model.
- Reuse generation byte-budget and permit ownership patterns from
  `manifold-wasm::resource_limits` for bounded protocol lanes.
- Implement state-machine unit tests and malformed-record property tests.
- Define endpoint-independent traits for backend commands and client events.
- Define snapshot adapters from `SurfaceSessionService` and backend window
  records without exposing `WlSurface` or other Smithay identities.
- Add diagnostics representations before adding a wire codec.

Exit criteria:

- stale leases and entity generations are rejected;
- snapshot/delta gaps force resynchronization;
- subscription/frame cleanup releases every owned permit deterministically;
- geometry round trips preserve the checked current conversion policy;
- all queues and payloads have byte and item bounds.

### Phase 1: in-process separation

- Add a typed backend presentation service for inventory revisions, lease state,
  subscriptions, and connection generations beside the existing services in
  `state_services.rs` or in a dedicated service module.
- Derive backend session/window inventory from `SurfaceSessionService` and the
  compositor's window records rather than creating a second session catalogue.
- Split the current `FocusService` semantics: viewport focus becomes client
  state, while backend hosted-window focus records only lease-authorized seat
  effects and stable producer identity.
- Replace direct presentation mutation of `Manifold` with typed in-process
  channels implementing the protocol model.
- Move viewport layout, visibility, hit testing, and chrome state into a client
  state object.
- Integrate attach, detach, and ordered teardown through `runtime.rs`; keep
  nested test orchestration in `nested_test.rs`.
- Preserve existing attached and offscreen behavior through the boundary.

Exit criteria:

- inventory, lease, and subscription transitions are atomic and fallible before
  Smithay effects are applied;
- the client half has no mutable reference to backend state or Smithay objects;
- closing the client half leaves hosted applications alive;
- a new client half reconstructs from an inventory snapshot;
- behavior tests exercise the same boundary used by later process separation.

### Phase 2: WGPU client renderer

- Build the portable WGPU scene compositor and platform-shell boundary.
- Keep raw display/window handles and WGPU surface creation behind a narrow
  typed adapter modeled on the audited ownership in `winit_presenter.rs`.
- Render synthetic panes, chrome, overlays, and copied RGBA window textures.
- Implement checked offscreen capture through the client renderer using shared
  extent and alpha/color policies.
- Keep the current GLES presenter available until the new path has behavioral
  and capture parity.
- Prove software rendering in the Nix behavior-test environment.

Exit criteria:

- native WGPU presentation and offscreen WGPU tests replace GLES for the new
  client path;
- device loss or forced renderer recreation rebuilds the scene;
- screenshots and scene diagnostics identify pane and frame revisions.

### Phase 3: Wayland frame broker and copied transport

- Snapshot committed toplevel surface trees independently of final composition.
- Capture or retain source content at commit/import time before Smithay consumes
  transient `BufferAssignment`; do not use the late debug-dump path as a broker.
- Reuse the current explicit-acquire and implicit DMA-BUF readiness decisions
  when establishing frame availability, while keeping renderer formats and EGL
  objects out of producer inventory.
- Implement copied-image subscription, damage, frame leases, and WGPU upload.
- Stop frame work when no subscription exists.
- Preserve application state and current inventory with zero clients.

Exit criteria:

- a real hosted Wayland application is streamed as an independent texture;
- SHM and DMA-BUF commits are observed at a stable owned boundary rather than a
  recover-later debug seam;
- detach and reattach restore its current content;
- slow subscribers do not create an unbounded `wl_buffer` or frame backlog;
- hide/unsubscribe does not close the application.

### Phase 4: lease-owned configuration and input

- Implement session presentation lease acquisition, generation, revocation, and
  diagnostics.
- Move final layout and hit testing into the client.
- Route resize and input through lease-validated protocol commands and shared
  checked geometry conversions; preserve typed key-policy invocation context.
- Separate local hide/close from backend application close/terminate.

Exit criteria:

- stale controller commands are rejected;
- a detached session retains last configured sizes;
- a reconnecting controller can resize and interact with real Wayland clients;
- an observer can view but not configure or inject application input.

### Phase 5: client WASM runtime

- Add explicit presentation/session actor placement and typed event-subscription
  metadata for core, Component, and Preview 1 package forms.
- Extract or reuse the existing generation resource limiter, byte budgets,
  mailbox accounting, reserved completion lanes, and structured worker teardown
  in the client host.
- Apply the proven replacement order: construct an unreachable candidate, fully
  validate it, stop and join the old generation, run the candidate start hook,
  and only then publish the candidate.
- Instantiate presentation actors per viewport.
- Move the existing typed key-policy invocation and immutable-publication lane
  to the client, and run layout policy locally.
- Route backend files, processes, PTYs, sockets, timers, streams, and watches
  through asynchronous remote capabilities with the same generation-owned
  handle and cancellation semantics as `RuntimeIoHub`.
- Add client-local persistence and remote actor/capability completion routing.
- Prove a real interactive client-side WASM pane with no network operation on
  key-to-frame updates.

Exit criteria:

- local actor messages and render updates remain local;
- backend file access completes asynchronously over the boundary;
- presentation actors have aggregate memory, table, host-byte, mailbox,
  completion, worker, and resource limits;
- reconnect produces explicit completion failures or application-level resume;
- actor replacement joins workers and operations and releases endpoint-local
  graphics before the replacement becomes reachable.

### Phase 6: local shared-memory process boundary

- Run the presentation client as a separate native process.
- Extend the DBus handshake to authenticate the endpoint and establish a fresh
  connection generation.
- Exchange size-sealed memfd regions and event-notification descriptors for one
  bounded single-producer/single-consumer direction per endpoint.
- Implement reserved critical, reliable ordered, latest-state, and registered
  bulk-resource lanes with explicit item and byte budgets.
- Keep DBus as the cold descriptor broker; do not add a dedicated local socket
  message transport.
- Add owner-loss and optional pidfd liveness handling, malformed-region
  validation, and generation-scoped cleanup.
- Preserve copied-image fallback as the correctness baseline.
- Extend `runtime.rs` ordered shutdown so connection operations, backend actor
  work, frame leases, client actors, and the client process are drained or
  joined in an explicit order.

Exit criteria:

- input, inventory, actor routing, subscription, frame-offer, and release traffic
  use shared memory after bootstrap;
- critical records remain deliverable while latest-state and frame lanes are
  saturated;
- either process can crash and restart without corrupting the other;
- stale mappings and resource IDs are rejected after reconnect;
- no hidden shared pointer or renderer object crosses the boundary;
- teardown releases all connection-owned frame and operation resources.

### Phase 7: DMA-BUF fast path

- Spike and select the WGPU external-image import strategy.
- Add generation-scoped reusable DMA-BUF pool registration through the DBus
  descriptor broker.
- Negotiate DRM device identity, formats, modifiers, planes, color metadata,
  and acquire/release synchronization.
- Register reusable synchronization timelines and carry per-frame acquire and
  release points through shared memory, including backend import of one-shot
  producer fences where necessary.
- Publish frame offers and releases by resource ID through shared memory without
  per-frame DBus calls.
- Compare direct hosted-client buffer retention with copies into backend-owned
  exportable pools and expose the selected path in diagnostics.
- Implement fallback on incompatible adapter or format.
- Measure copies, GPU residency, frame latency, and retained buffers.

Exit criteria:

- supported Linux configurations use the direct path;
- unsupported configurations transparently retain correctness through copy;
- explicit synchronization and release are verified with real workloads;
- no DMA-BUF assumption leaks into pane, actor, or renderer APIs.

### Phase 8: remote and web clients

- Implement an authenticated encrypted remote carrier, initially targeting QUIC
  streams for reliable/bulk lanes and datagrams for eligible latest-state
  records.
- Preserve reliable delivery for key/button/focus transitions, settled resize,
  lease changes, actor messages, and terminal operation results.
- Add at least one bounded per-window remote frame transport with recovery
  semantics; evaluate encoded video and lossless damage transport against real
  browser, terminal, editor, and media workloads.
- Compile the shared client core and renderer for web WASM/WebGPU.
- Add platform adapters for browser input, storage, and networking.

Exit criteria:

- remote layout, local actors, and plugin graphics remain client-side;
- only subscribed managed-window content consumes frame bandwidth;
- disconnection and congestion do not stall backend applications or actors;
- web capability fallback is explicit and diagnosable.

### Phase 9: freeze plugin UI and graphics baselines

- Implement materially different client-side WASM workloads.
- Finalize component, semantic-tree, render-packet, text, and graphics-resource
  contracts against the proven WGPU client.
- Freeze packet encodings only after parser fuzzing and cross-platform replay.

## Verification Strategy

Tests should use real workloads in addition to protocol unit tests.

Required coverage includes:

- continued passage of the merged state-service, geometry, safe-boundary,
  runtime-I/O, typed-key-policy, actor-lifecycle, harness-lifecycle, and exact
  nested-child ownership proofs while their responsibilities are moved;
- lease grant, release, disconnect, stale generation, and observer rejection;
- complete inventory snapshot, delta gap, and reconnect resynchronization;
- zero-client session survival and reattach;
- demand subscription, suspension, unsubscribe, and final-subscriber behavior;
- bounded slow-client frame supersession;
- surface trees with subsurfaces, damage, scaling, and popups;
- interactive resize through real xdg configure/ack/commit;
- keyboard, pointer, click, and scroll routed from the real client;
- DBus bootstrap followed by shared-memory-only steady-state protocol traffic;
- shared-memory lane saturation, coalescing, reserved critical delivery, peer
  crash, and stale-generation cleanup;
- copied transport on software WGPU;
- DMA-BUF pool registration, shared-memory frame references, direct path, and
  forced fallback where available;
- client and backend process crash/restart;
- local presentation actor key-to-frame latency without network messages;
- asynchronous backend file operation from a presentation actor;
- WGPU device recreation and client actor graphics reconstruction;
- screenshot assertions tied to pane, inventory, and frame revisions;
- bounded memory/resource samples during repeated attach/detach and subscribe
  churn.

Automated graphical verification continues to use the built-in Nix-backed test
CLI and offscreen outer supervision. Tests record correlated metrics from the
backend and presentation client. Host-visible manual harnesses are not the
automated acceptance path.

## Deliberately Deferred Decisions

The following do not block the initial in-process, copied-frame architecture:

- exact record codec within the fixed local shared-memory carrier;
- final QUIC, WebTransport, or other authenticated remote carrier selection;
- final package signing and remote plugin installation UX;
- exact DMA-BUF-to-WGPU integration mechanism;
- video codec and congestion-control policy;
- independent per-window controller leases;
- multiple simultaneous interactive Wayland seats;
- cross-client synchronized viewport layouts;
- actor migration between backend and client;
- final plugin render-packet opcode encoding;
- whether selected client-local preferences are synchronized by a generic
  service or only through application-specific session actors.

These decisions must preserve the ownership, generation, lease, and bounded
lifecycle rules above.

## Related

- [[../concepts/presentation-surface]]
- [[../concepts/session]]
- [[../concepts/pane]]
- [[../concepts/plugin]]
- [[wayland-compositor]]
- [[plugin-runtime]]
- [[plugin-io-and-resources]]
- [[plugin-ui]]
- [[plugin-graphics]]
- [[input-shortcuts]]
- [[layout-policy]]
- [[testing]]
