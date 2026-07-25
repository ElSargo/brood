---
status: canonical
type: implementation-map
authority: product
last-reviewed: 2026-07-24
code-links:
  - crates/manifoldd/src/runtime.rs
  - crates/manifoldd/src/compositor.rs
  - crates/manifoldd/src/control.rs
  - clients/manifold-client/src/main.rs
  - clients/manifold-client/src/actor_runtime.rs
  - crates/manifold-layout/src/lib.rs
---

# Code Architecture

This page maps the external-only presentation design to the source tree. Exact
CLI and ABI details belong to their defining code and are not duplicated here.

## Process Boundaries

### Session server

`manifold-sessiond` owns:

- the Smithay nested Wayland display;
- surface sessions, hosted applications, and stable managed-pane identities;
- session-scoped actors, workers, resources, and control services;
- revisioned window inventory and per-window frame production;
- presentation handshake and bounded transport lanes;
- lease-authorized pane-local input, focus, and content-size configuration.

It remains useful with no presentation client. It has no Winit event loop,
presentation renderer, viewport layout, chrome, or global hit testing.

### Presentation client

`manifold-client` owns:

- its Winit window, WGPU device, swapchain, and composition loop;
- client init and presentation actors;
- the privileged synchronous key-policy provider;
- local plugin panes and their graphics resources;
- the recursive layout-lease tree and accepted composed layout;
- local focus, hit testing, pointer grabs, and physical key state;
- frame import and managed-pane size requests.

The user-facing `manifold` command launches or focuses this process and uses
the server only as the session authority.

## Presentation Transport

DBus bootstraps the local connection and descriptor exchange. Bounded
shared-memory lanes carry scene inventory, input, configuration, and actor
messages. DMA-BUF is the preferred per-window image transport; copied images
remain a correctness fallback.

The server publishes each managed surface tree as an independent window frame.
The client returns a requested logical content size for a pane. Placement,
clipping, z order, sidebar width, and presentation-local focus never cross the
connection.

## Recursive Layout

`manifold-layout` is portable and compositor independent. The client creates a
root layout lease and may grant one recursive `rest` child per node. Each
holder receives only its viewport, pane allocation, focus context, and current
revision.

Actors process `layout-state` through ordinary mailboxes and submit candidates
through their ordinary host outbox. The client authenticates the sender,
generation, lease, revision, and pane ownership before composing the complete
tree. It then canonicalizes transforms and z order, clips children, selects
focus, builds hit-test regions, and derives managed-pane size requests.

The default client init actor starts the session switcher and the initial
coding-agent UI as independently addressed children. The switcher claims the
root, places its pane on the right, and delegates `rest` to the workspace
actor, where the agent UI and session windows are composed. Its visible state
belongs to the plugin. The key policy sends ordinary sidebar and new-agent-UI
actions to the init actor; the relevant actor updates its child graph or
republishes its layout fragment. Session-catalogue changes rebind each agent UI
to the active session's agent registry.

## Input

Physical input enters the client:

1. reserved recovery handling;
2. the synchronous key-policy provider;
3. ordinary actor messages returned by that policy;
4. focused local plugin delivery or managed-pane forwarding.

Pointer targeting uses the accepted client layout. Managed input crosses the
protocol with pane-local coordinates. The server resolves only the addressed
Wayland surface tree. It does not repeat viewport hit testing.

## Focus And Lifecycle

Client focus is one local pane identity. Managed focus changes are forwarded
to the session server; plugin focus stays in the client runtime.

Closing or crashing the client revokes its presentation and layout leases but
does not close the server, surface session, or hosted applications. A
replacement client receives the current inventory, creates fresh lease
generations, and reconstructs its own presentation.

Actor replacement is generation scoped. Late mailbox results and layout
submissions from an old generation are rejected. Failure of a recursive wrapper
may promote its live `rest` child while the wrapper's pane follows ordinary
plugin lifecycle.

## Diagnostics And Testing

The server reports session state, managed-window lifecycle, frame export,
protocol, and accepted pane-local input/configuration. The client reports
presentation actors, lease generations, accepted layout revisions, frame
imports, composition, hit testing, and presentation timing.

Composed screenshots and layout manifests are client-owned artifacts. Server
debug output may include raw surface or session evidence but is not the final
pixel or placement authority.

The primary graphical proofs are:

- `tools/test-client-layout-actor.sh`;
- `tools/test-session-switcher-plugin-nested.sh`;
- the software-rendered external-client E2E check.

## Related Pages

- [[architecture]]
- [[systems/runtime]]
- [[systems/presentation-client]]
- [[systems/layout-actor]]
- [[systems/input-shortcuts]]
- [[systems/testing]]
