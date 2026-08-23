---
status: target
type: system
security-boundary: mixed
runtime-boundary: process-and-network
wire-format: not-frozen
authority: product
last-reviewed: 2026-08-09
---

# Presentation Client Architecture

## Purpose

Brood separates the durable session server from the software that presents
and interacts with it. A presentation client is an independent viewport over
the available window catalog. It owns layout, interaction state, presentation
actors, GPU resources, and final rendering. Sessions remain grouping and
sandbox boundaries, not pane-visibility boundaries.

The normal case is zero or one interactive presentation client. A session
continues running with no client. Additional observers are permitted, but only
the current presentation lease holder may configure or send primary input to
hosted applications.

## Ownership

The session server owns:

- session and managed-application identity;
- hosted Wayland protocol state and stable managed-pane identities;
- durable session actors, resources, and operations;
- revisioned pane inventory and demand-driven per-window frame production;
- presentation connection and lease authority;
- validation of managed-pane identity, input, and requested content sizes.

The presentation client owns:

- its native or web viewport and platform input state;
- presentation-local actors, workers, publications, and persistence;
- synchronous key- and pointer-policy leases;
- local plugin panes, canvas resources, and native text services;
- the recursive layout-lease tree, visibility, focus, chrome, and animation;
- resize epochs, projected geometry, bounded staged content, and atomic
  promotion;
- final WGPU composition, hit testing, accessibility projection, and capture;
- managed-window subscriptions and frame import.

The client receives every available managed-window identity together with its
session membership and application identity. This catalog is independent from
frame readiness. A session switch changes the client's default visible subset
and immediately publishes a new layout-state revision; accepted placement is
shown at once, with client-rendered placeholders until the selected windows'
frames arrive. Layout actors may instead choose any authorized catalog entries,
including windows from more than one session.

No final placement rectangle, z order, clipping rule, sidebar geometry, or
viewport hit-test result crosses the server boundary.

## Required Invariants

- The server remains coherent with zero connected clients.
- The server never creates a native or offscreen *human presentation target*.
  A server-owned computer-use automation render target is permitted, but it
  cannot accept presentation layout authority or include client-local panes.
- Closing, crashing, or replacing a client does not terminate a session or its
  managed applications.
- A session with no managed windows remains alive until it is explicitly
  closed; plugin panes, actors, agents, processes, and other session resources
  are valid content.
- Viewport state does not become backend-owned merely because a client may
  persist preferences through an explicit session operation.
- Only the current presentation lease generation may configure or send primary
  input to hosted applications.
- Stale connections, actor generations, resource handles, layout leases, and
  inventory revisions cannot affect replacements.
- Immediate input and rendering never wait synchronously for backend I/O.
- Unrelated session work never waits synchronously for client policy or
  rendering.
- Reliable transitions and supersedable latest-state/frame traffic have
  separate bounded flow-control rules.
- WGPU resources, platform handles, and plugin graphics objects never cross the
  presentation protocol.
- Rendered dividers, interaction hit testing, and pane content always use one
  presented layout revision.
- A waited pane's partially staged content never replaces visible content.

## Topology

```text
session server
  session and window registry
  hosted Wayland compositor
  session actors and resource services
  per-window frame broker
  presentation lease and protocol endpoint
             |
             | local: DBus bootstrap
             |        bounded shared-memory lanes
             |        DMA-BUF or copied window frames
             |
             | remote: authenticated carrier
             |         encoded or damage-based window frames
             |
presentation client
  connection and inventory mirror
  presentation actors and key policy
  recursive layout leases
  plugin panes and graphics
  WGPU compositor and platform shell
```

Presentation actors run in the client for local and remote deployments. Their
mailbox traffic, layout updates, UI state, animations, and drawing remain local.
Only explicitly addressed operations and daemon- or session-realm actor
messages cross the connection. A presentation child declares one immutable
presentation scope when its init actor creates it: either `global` or an
explicit backend-qualified session. The client does not derive ownership from
the active session, focused backend, or actor ID.

The default visible set contains presentation-global actors plus actors and
managed windows associated with the one selected backend/session pair.
Connections retain their independent active-session catalogues for routing and
reconnection, but those per-backend values do not make every backend's terminal
frontend visible. Layout actors that intentionally implement multi-session
views consume the qualified global catalogue explicitly.

## Connection And Lease Lifecycle

The private local IPC socket is the discovery, authentication, control, and
descriptor broker for Brood-owned same-machine endpoints. D-Bus is only the
public message and control surface for external applications. A successful
private handshake creates a fresh connection generation, immutable
negotiated capabilities, bounded protocol lanes, and optional presentation
lease. Product actors use addressed mailbox traffic over those lanes to reach
daemon and session actors rather than calling the local DBus control API.

Carrier and frame transport are negotiated independently. The three standard
profiles are local shared-memory lanes with DMA-BUF, local shared-memory lanes
with shared images, and authenticated network lanes with network images. All
profiles carry the same semantic messages. See [[presentation-transport]].

Disconnect immediately revokes connection-owned authority and releases frame
subscriptions and transport resources. It does not close window producers. A
replacement client receives a complete inventory snapshot and current window
content, then creates fresh presentation-actor and layout-lease generations.

The same rule applies to a terminal presentation: disconnect destroys its
client-local canvas, text resources, and presentation actor, but not the shell
PTY owned by the session actor. A replacement terminal presentation attaches to
that actor and receives a current revision before accepting pane input.

Observers may mirror inventory and subscribe to frames without gaining input or
configuration authority.

## Window Inventory And Frames

Window inventory describes stable producer identity, session membership,
application identity, committed content extent, and transport capabilities.
Snapshots are revisioned; a delta gap forces resynchronization.
Inventory invalidation is event driven. Periodic snapshot comparison is only a
recovery mechanism and must never determine user-visible interaction latency.
Local clients receive atomic catalog snapshots through a dedicated
server-to-client shared-memory lane with eventfd wakeups. Correlated control and
session operations use their own bounded shared-memory request/response lanes;
descriptor bootstrap uses the private packet socket. Neither path enters the
public D-Bus dispatcher.

Client-realm Component actors wake the native presentation loop when an
observer event is admitted, and the loop drains those events before sleeping
again. A dormant Component actor therefore does not arm a periodic client
timer. Preview 1 compatibility actors may use a bounded recovery poll until
they provide the same notification boundary. When a bounded presentation lane
is full, the client retries on a short deadline instead of busy-spinning the
platform event loop; critical input ordering remains intact while a slow or
descheduled consumer cannot pin a CPU.

Frame work is demand driven. Each subscription has bounded in-flight ownership
and explicit acquire, release, cancellation, and disconnect behavior.
Intermediate frames may be superseded, but reliable inventory and resource
transitions may not.

DMA-BUF is a preferred local optimization rather than the frame model. Copied
images are the correctness baseline. Remote transports preserve per-window
identity so the client still owns final layout and composition.

## Managed Resize Epochs

Protocol version 22 correlates a synchronized managed resize from the accepted
client layout to one exact server commit snapshot. Before acceptance, every
named managed wait pane must resolve to a current backend route and connection
generation. The fixed wait set can contain plugin and managed panes. All
affected managed panes receive the epoch; only named panes block promotion.

For each managed pane, the client records the route, epoch, target pixel extent,
logical viewport, and `scale_120`. It derives each logical dimension with
checked integer rounding of `pixel * 120 / scale_120`. The server configure,
status, committed buffer, exported snapshot, and returned frame must preserve
that tuple. A committed buffer can provide more effective source samples than
the target. The server captures an exact target-sized immutable snapshot; the
client requires an exact zero-origin source rectangle and resource extent for
that snapshot.

An exact tagged frame stages one generation-scoped resource for its active
pane and epoch. A replacement exact frame releases the prior stage. A tagged
frame for any other epoch is dropped, including after supersession, timeout,
abort, or promotion. While an expectation is active, an untagged frame cannot
replace visible content. Plugin and managed stages promote together only after
the fixed wait set and recursive region dependencies are final. A managed pane
that is affected but not waited can stage without becoming a barrier.

Timeout discards every waited managed stage and keeps the complete old visible
set. If a pane has no old resource, the native placeholder is explicit recovered
content. The client records whether each recovered pane uses `old-projected` or
`placeholder`, retires the epoch floor, and sends one untagged current-size
request. The server waits for a later ordinary commit before it resumes scene
publication. Therefore the already-current mixed surface set cannot replace
the observable degraded frame.

Supersession releases old stages and creates fresh expectations from durable
content. Pane removal or backend-generation loss aborts a target layout that
names the unavailable pane, clears its barrier and resources, reports the exact
bounded unavailable-pane set, and keeps the durable layout. A hidden inventory
pane that the target does not name does not abort the transaction. An accepted
transaction otherwise converges independently of capture or provider lifetime.

## Recursive Layout

The client combines managed windows and local plugin panes in one pane
namespace. It mints the root layout lease and grants recursive child leases.
The first implementation supports one delegated `rest` child per node.

Each actor sees only its allocated viewport, owned panes, focus context, lease
generation, and inventory revision. It submits a lease-local candidate through
its ordinary host outbox. The client authenticates the holder, validates pane
authority and revision, recursively composes the complete tree, clips child
subtrees, canonicalizes paint order, and selects an eligible focus request.
Catalog and session changes publish a new inventory revision without replacing
the currently accepted actor layout. The client keeps that layout and its
relevant window resources visible until the complete replacement layout is
accepted, then swaps layouts atomically and releases panes the replacement no
longer references. Native fallback is startup and failure recovery, not an
intermediate session-switch state.

Presentation-global panes may appear in any session view. A session-scoped
plugin pane remains owned by its declared session across activation and
catalogue changes and is omitted from the default visible set when another
session is selected. The complete pane inventory and scope remain available to
custom multi-session layout policy. Explicit session closure stops its scoped
presentation actors; merely having no managed windows does not.

The default session switcher claims the root, places its pane on the right, and
delegates `rest` to the workspace actor. Hiding and showing are ordinary
switcher actor actions that republish the root fragment; the client has no
switcher-specific visibility mechanism.

An accepted layout rectangle is also the sizing authority for a local plugin
pane. The client converts that physical rectangle through the current output
scale for pane resize events, replays the retained graphics packet into a
matching physical canvas, and routes pointer coordinates in the same pane-local
logical space. Presentation-window dimensions are not substituted for an
individual pane's dimensions.

See [[layout-actor]].

## Input

Physical input is client-owned. Reserved recovery behavior runs first, followed
by the current synchronous key policy, temporary input capture, and focused
pane delivery.

Before focused-pane delivery, the client normalizes named keys and modifiers
into the stable plugin event contract. Ctrl, Alt, Shift, and Super use bits 1,
2, 4, and 8 respectively; this is deliberately the same modifier convention as
the actor key-policy ABI. Printable text remains a separate committed-text
event.

The policy returns a bounded decision and ordinary mailbox-message batch.
Long-running or stateful work continues in actors. A managed target crosses the
protocol only after client hit testing, with a pane identity and pane-local
coordinates. The server resolves that pane's Wayland surface tree and applies
seat effects under the presentation lease.

See [[input-shortcuts]].

## Presentation Plugins

Client init selects the presentation shell and may nominate policy providers.
Presentation actors use the same generation, mailbox, completion, worker, and
resource ownership rules as session actors, but their capabilities and
lifecycle are local to the client.

Backend file, process, network, PTY, timer, stream, and durable actor operations
are asynchronous remote capabilities. Client-local UI and rendering do not
require a backend round trip.

The first-party terminal deliberately splits across that boundary. Its session
actor owns the PTY and terminal state; its presentation actor owns input routing
and rendering. Neither side gains compositor, platform-window, or WGPU
internals.

See [[terminal-plugin]].

## Rendering

WGPU is the only final presentation renderer. Native and web shells adapt
window creation, input, accessibility, and external-image import without
exposing their handles to plugins or the protocol.

The client composes imported managed-window textures and local plugin scenes in
linear color space, applies the accepted recursive layout, and owns final
presentation timing and screenshots. Device recreation rebuilds imported and
plugin graphics from retained logical state.

## Diagnostics

Server diagnostics expose:

- active connection and presentation lease generations;
- inventory revision and resynchronization;
- per-window subscribers, committed extents, and frame export;
- transport selection, buffer ownership, and release;
- rejected input, configuration, actor, and capability operations.

Client diagnostics expose:

- connection and mirrored inventory revision;
- layout lease holders, generations, revisions, and rejection reasons;
- visible panes, focus, hit testing, and subscriptions;
- import, upload, render, and presentation timing;
- actor latency, mailbox pressure, key-policy timing, and graphics failures.

Correlated traces use stable connection, session, window, subscription, frame,
actor, lease, and operation identifiers.

## Verification

Acceptance uses real external-client workloads:

- zero-client session survival and replacement-client recovery;
- recursive root/rest lease composition, stale generation rejection, wrapper
  failure promotion, and disjoint pane authority;
- the session switcher's visible, hidden, and restored right-sidebar states;
- real keyboard, pointer, click, and scroll routed through client hit testing;
- interactive Wayland resize with requested, configured, and imported extents;
- interactive native viewport resize with continuous buffer scaling and one
  settled actor, plugin, and managed-client size revision;
- shared-memory saturation without loss of critical transitions;
- copied transport under software WGPU;
- DMA-BUF import and synchronization where supported;
- device recreation and presentation-actor graphics reconstruction;
- frontend-owned screenshots tied to layout and inventory revisions.

Automated graphical tests run the real client under an isolated compositor.
Backend-composed frames and backend layout status are not valid substitutes.

## Deferred Decisions

The following remain open without changing the ownership model:

- damage, video, and GPU-aware remote frame codecs beyond the lossless
  correctness baseline;
- final plugin packaging and signing policy;
- exact DMA-BUF-to-WGPU import strategy on every platform;
- simultaneous independent controller leases;
- multiple interactive Wayland seats;
- cross-client synchronized viewport preferences;
- migration of live actors between server and client;
- the future advanced-graphics protocol.

## Related Pages

- [[../../systems/runtime]]
- [[layout-actor]]
- [[input-shortcuts]]
- [[plugin-runtime]]
- [[plugin-ui]]
- [[plugin-graphics]]
- [[../../systems/testing]]
- [[../../frictions/terminal-plugin]]
