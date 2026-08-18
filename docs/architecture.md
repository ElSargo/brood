---
status: canonical
type: system-map
authority: implementation
last-reviewed: 2026-07-24
code-links:
  - crates/broodd/src/lib.rs
  - crates/broodd/src/compositor.rs
  - crates/broodd/src/control.rs
  - crates/broodd/src/state.rs
  - crates/brood-wasm/src/module.rs
  - crates/brood-wasm/src/component_actor.rs
  - crates/brood-wasm/src/preview1_worker.rs
---

# Architecture

Brood is organized around a product-level [[concepts/workspace]] containing
isolated [[concepts/session]]s. Session authority and presentation are separate
process boundaries.

## Current Components

`brood`

The user-facing binary. It parses commands, starts runtime sessions, starts nested test topologies, and sends DBus control commands.

`brood-sessiond`

The authoritative session server. It owns the nested Wayland compositor,
hosted applications, session catalogue, durable actors, and control endpoint.
It never creates a human presentation window or computes human viewport
layout. The server may own a fixed computer-use automation projection that is
independent from presentation layout.

`brood-client`

The portable presentation frontend. It owns its Winit/WGPU viewport,
presentation actors, recursive layout leases, composition, hit testing, local
focus, and physical input policy.

`brood-runner-exec`

The sidecar command runner used for entered shell or command execution inside a session runner context.

`brood-runner-nix-develop`

The built-in Nix session runner. It caches a sourceable `nix print-dev-env --profile` activation script and then runs the supplied inner binary.

## Runtime Boundaries

The session server owns:

- nested Wayland compositor state
- hosted app surface mapping
- session and surface-session state
- startup module and startup app loading
- DBus control interfaces
- session-scoped actors and workers
- Component Model and Preview 1 actor/worker runtimes with completion I/O and
  notification-based coordination
- async WASM test harness lane

The presentation client owns viewport state, client-side WASM actors, local
actor mailboxes and key policy, recursive layout, and WGPU composition. The
session process exports window inventory, lease-validated input and size
configuration, backend capabilities, cross-runtime actor routing, and
demand-driven frame streams. Local clients bootstrap through DBus, then use
bounded shared-memory protocol lanes and DMA-BUF or copied per-window surfaces.
Remote profiles preserve the same semantic ownership over an authenticated
carrier. See [[systems/presentation-client]] and
[[decisions/0007-presentation-transports-and-client-actors]].

## Data Flow

Hosted Wayland apps connect to the nested Wayland compositor. The server tracks
their surfaces as stable panes and publishes per-window frames. The client
imports those frames, combines them with local plugin panes, recursively
resolves its layout leases, and composites the viewport. Closing the client
revokes presentation authority without closing hosted applications.

## Control Flow

User-facing commands such as `enter`, `show`, `hide`, `list`, `switch`, `plug`, and `ctl` resolve a DBus control target. The default target is the entered session when available, otherwise the global Brood session server. See [[systems/dbus-control]] and [[reference/cli]].

## Plugin And Extension Boundary

Installed plugins are trusted code inside a session environment. Actors provide
public addressing, separate address spaces, lifecycle, and fault containment;
private workers share actor memory and may wait on ordinary WASI without
blocking actor dispatch. Policy providers handle immediate bounded decisions
from immutable publications. Authority comes from the enclosing local,
containerized, or remote execution environment, not from a security boundary
between individual plugins.

WASM and extension APIs should operate on workspace, session, pane, command, status, input, and layout concepts. They should not expose Smithay, Winit, EGL, DMA-BUF, or renderer internals as the product API.

Each host owns the invariants within its boundary. The server validates session
and pane lifecycle, input/configuration leases, and durable effects. The client
validates layout leases, final composition, hit testing, local focus, and key
policy results.

See [[systems/presentation-client]], [[systems/plugin-runtime]], [[systems/input-shortcuts]], [[systems/layout-actor]], [[systems/plugin-ui]], [[systems/plugin-graphics]], [[systems/plugin-advanced-graphics]], and [[systems/extension-host]].
