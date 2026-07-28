---
title: Multi-backend presentation
status: accepted
type: architecture
security-boundary: process-and-network
runtime-boundary: process-and-network
---

# Multi-backend presentation

Brood presentation is a client-wide view over a registry of independently
connected session backends. A backend may be a local session server, a remote
machine, or a relay into a container. The registry owns connection lifecycle,
generation validation, and routing; the renderer and presentation actors do
not infer a backend from a pane or session number.

`BackendId` is stable for the lifetime of a backend identity. Connection
generations remain separate: reconnecting invalidates resources and leases for
that connection without invalidating unrelated backends. Session and pane
numbers are backend-local, so the view model must use `(BackendId, id)` keys.

The semantic protocol remains shared by every carrier. Each connection keeps
its critical, interactive actor, reliable, latest-state, bulk, and
frame-resource lanes independent. Actor streams isolate daemon traffic and
session shards while preserving order within a session mailbox. Backend A
never addresses backend B through a guest-supplied identifier; the registry
selects the destination connection and stamps the connection-owned identity on
inbound events.

The view maintains a global pane catalogue with backend-qualified pane keys.
Layouts consume that catalogue and may compose panes from any subset of
backends. Focus is a qualified pane key, and input is routed through the
focused pane's backend connection. A missing or reconnecting backend leaves a
stable placeholder in the accepted layout while its catalogue and frame
resources recover.

Layout-state snapshots retain the backend and session realm for managed and
client-local panes. The default workspace maps only presentation-global panes
and panes belonging to the one selected `(BackendId, session)` pair. Attaching
another backend does not make its active session part of the default view, and
switching sessions removes the previous session's terminal and other
realm-bound panes without stopping their actors. A custom layout actor may
deliberately compose several sessions or backends from the global catalogue,
but it never needs to infer ownership from numeric pane or session IDs.

During the ABI migration, the client assigns each qualified managed pane a
stable presentation-local view handle. Existing layout and WASM messages may
continue to carry their `u64` handle, while the input boundary translates it
back to the owning backend-local pane before transport. Plugin panes remain
presentation-local and occupy a separate handle range. The handle registry is
therefore an implementation bridge, not a claim that backend-local pane IDs
are globally unique.

Presentation actor addresses are backend-qualified at the ABI boundary.
Session-realm messages carry `(BackendId, session, actor)` and daemon-realm
messages carry `(BackendId, actor)`. A session-associated presentation child
records the same backend and session when it is created. The host rejects
cross-backend use of that association, while presentation-global actors may
address any backend currently present in the registry. Inbound mailbox sender
identity retains the authenticated backend and realm instead of collapsing
equal session or actor names from different servers.

Presentation size, focus loss, keyboard reset, and disconnect are
connection-wide state and are published to every backend. Pane-targeted input
and frame-presented acknowledgement are routed only to the owning backend.
If one remote transport stops while another backend remains connected, the
client removes that backend's live routes and realm-bound frontend actors
without terminating the presentation. The last accepted actor layout may keep
its panes as placeholders until the operator reconnects or publishes a
replacement composition.

The migration order is:

1. qualify protocol snapshots, scenes, actor events, and connection
   generations with `BackendId`;
2. replace the singular client connection bundle with a backend registry;
3. move renderer inventory, focus, frame resources, and input routing to
   qualified pane keys;
4. make client actors and layout state consume the global catalogue; and
5. add local-plus-remote and two-local-backend behavior proofs.

## Session creation

The sidebar's `New` action is backend-aware. It opens the fuzzy picker with
the currently attached backends plus the backend-creation actions supported by
the current client installation. The remote action accepts an OpenSSH target,
deploys and starts the remote backend asynchronously, and attaches its
authenticated transport to the running presentation. Selecting an attached
backend asks its daemon actor to enumerate a bounded directory catalogue rooted
at the backend user's home, filesystem root, and known session directories.
The returned paths populate the same fuzzy picker; selecting one sends the
create operation to that backend, where the backend owns session creation and
returns the new session identity.

When no name is supplied, the owning backend generates a random readable name
with an entropy suffix. Session numbers do not seed names, so independently
started local and remote backends do not deterministically present the same
initial name.

Creating a local backend attaches another session-server connection to the
same presentation. Connecting a remote backend starts a packaged SSH deployer
whose lifetime pipe belongs to the client, then attaches the returned
authenticated endpoint. Closing the client or disconnecting that backend
closes the pipe and stops the remote session. Both paths publish the resulting
backend and its session inventory through the same registry, so the view does
not need a separate local or remote creation path.

The packaged SSH deployment workflow keeps both the deployed remote backend
and the ordinary local backend connected. Each connected backend's active
session receives a realm-bound frontend for the default terminal actor, but
only the globally selected session's frontend participates in the default
layout. Switching back restores the preserved frontend; explicit multi-session
layout actors remain free to map several connected backends at once.

The exact message shapes and parser behavior remain defined by
`crates/brood-protocol/src/lib.rs`; this document records ownership and
composition semantics rather than duplicating argument or wire-format lists.
