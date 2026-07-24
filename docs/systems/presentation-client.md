---
status: canonical
type: system
security-boundary: mixed
runtime-boundary: process-and-network
wire-format: not-frozen
authority: product
last-reviewed: 2026-07-24
---

# Presentation Client Architecture

## Purpose

Manifold separates the durable session server from the software that presents
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
- the synchronous key-policy lease;
- local plugin panes, canvas resources, and native text services;
- the recursive layout-lease tree, visibility, focus, chrome, and animation;
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
- The server never creates a native or offscreen presentation target.
- Closing, crashing, or replacing a client does not terminate a session or its
  managed applications.
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
Only explicitly addressed session operations and actor messages cross the
connection.

## Connection And Lease Lifecycle

DBus is the local discovery, authentication, and descriptor broker. A
successful handshake creates a fresh connection generation, immutable
negotiated capabilities, bounded protocol lanes, and optional presentation
lease.

Disconnect immediately revokes connection-owned authority and releases frame
subscriptions and transport resources. It does not close window producers. A
replacement client receives a complete inventory snapshot and current window
content, then creates fresh presentation-actor and layout-lease generations.

Observers may mirror inventory and subscribe to frames without gaining input or
configuration authority.

## Window Inventory And Frames

Window inventory describes stable producer identity, session membership,
application identity, committed content extent, and transport capabilities.
Snapshots are revisioned; a delta gap forces resynchronization.
Inventory invalidation is event driven. Periodic snapshot comparison is only a
recovery mechanism and must never determine user-visible interaction latency.
Local clients receive atomic catalog snapshots through a dedicated
server-to-client shared-memory lane with eventfd wakeups. DBus remains the
descriptor bootstrap and asynchronous command path, not the catalog transport.

Frame work is demand driven. Each subscription has bounded in-flight ownership
and explicit acquire, release, cancellation, and disconnect behavior.
Intermediate frames may be superseded, but reliable inventory and resource
transitions may not.

DMA-BUF is a preferred local optimization rather than the frame model. Copied
images are the correctness baseline. Remote transports preserve per-window
identity so the client still owns final layout and composition.

## Recursive Layout

The client combines managed windows and local plugin panes in one pane
namespace. It mints the root layout lease and grants recursive child leases.
The first implementation supports one delegated `rest` child per node.

Each actor sees only its allocated viewport, owned panes, focus context, lease
generation, and inventory revision. It submits a lease-local candidate through
its ordinary host outbox. The client authenticates the holder, validates pane
authority and revision, recursively composes the complete tree, clips child
subtrees, canonicalizes paint order, and selects an eligible focus request.

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
- shared-memory saturation without loss of critical transitions;
- copied transport under software WGPU;
- DMA-BUF import and synchronization where supported;
- device recreation and presentation-actor graphics reconstruction;
- frontend-owned screenshots tied to layout and inventory revisions.

Automated graphical tests run the real client under an isolated compositor.
Backend-composed frames and backend layout status are not valid substitutes.

## Deferred Decisions

The following remain open without changing the ownership model:

- final remote carrier and frame encoding;
- final plugin packaging and signing policy;
- exact DMA-BUF-to-WGPU import strategy on every platform;
- simultaneous independent controller leases;
- multiple interactive Wayland seats;
- cross-client synchronized viewport preferences;
- migration of live actors between server and client;
- the future advanced-graphics protocol.

## Related Pages

- [[runtime]]
- [[layout-actor]]
- [[input-shortcuts]]
- [[plugin-runtime]]
- [[plugin-ui]]
- [[plugin-graphics]]
- [[testing]]
