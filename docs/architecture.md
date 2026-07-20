---
status: canonical
type: system-map
authority: implementation
last-reviewed: 2026-07-20
code-links:
  - crates/manifoldd/src/lib.rs
  - crates/manifoldd/src/compositor.rs
  - crates/manifoldd/src/control.rs
  - crates/manifoldd/src/state.rs
  - crates/manifold-wasm/src/module.rs
  - crates/manifold-wasm/src/component_actor.rs
  - crates/manifold-wasm/src/preview1_worker.rs
---

# Architecture

Manifold is organized around a product-level [[concepts/workspace]] containing isolated [[concepts/session]]s. The current prototype keeps the session process, nested Wayland compositor, DBus control endpoint, and local presentation endpoint in one binary so the detach/reattach and nested-app lifecycle risks can be proven with fewer moving parts.

## Current Components

`manifold`

The user-facing binary. It parses commands, starts runtime sessions, starts nested test topologies, and sends DBus control commands.

`manifold-sessiond`

The sidecar session server binary used by [[concepts/session-runner]] activation. It re-enters the same runtime path after the runner has activated a project environment.

`manifold-runner-exec`

The sidecar command runner used for entered shell or command execution inside a session runner context.

`manifold-runner-nix-develop`

The built-in Nix session runner. It caches a sourceable `nix print-dev-env --profile` activation script and then runs the supplied inner binary.

## Runtime Boundaries

The current `manifold` runtime owns:

- nested Wayland compositor state
- hosted app surface mapping
- native Winit/GLES presentation attach
- offscreen presentation attach for tests
- session and surface-session state
- startup module and startup app loading
- DBus control interfaces
- core-WASM actors and synchronous key-policy providers
- Component Model and Preview 1 actor/worker runtimes with completion I/O and
  notification-based coordination
- async WASM test harness lane

The target architecture splits presentation into an independent portable client. The client owns viewport state, client-side WASM actors, local actor mailboxes and policy, and WGPU composition; the session process exports window inventory, lease-validated input/configuration, backend capabilities, cross-runtime actor routing, and demand-driven frame streams. The split is introduced first as a typed in-process boundary. The local process profile uses DBus to bootstrap bounded shared-memory protocol lanes and DMA-BUF or copied per-window surfaces. Remote profiles map the same semantic lanes onto an authenticated network carrier and encoded or damage-based per-window surfaces. See [[systems/presentation-client]] and [[decisions/0007-presentation-transports-and-client-actors]].

## Data Flow

Hosted Wayland apps connect to Manifold's nested Wayland compositor. Manifold tracks their surfaces as panes in the active session. The presentation endpoint imports or uploads those surfaces and composites the visible session. Closing the presentation window detaches presentation state while hosted apps and session state remain alive.

## Control Flow

User-facing commands such as `enter`, `show`, `hide`, `list`, `switch`, `plug`, and `ctl` resolve a DBus control target. The default target is the entered session when available, otherwise the global Manifold session server. See [[systems/dbus-control]] and [[reference/cli]].

## Plugin And Extension Boundary

Installed plugins are trusted code inside a session environment. Actors provide
public addressing, separate address spaces, lifecycle, and fault containment;
private workers share actor memory and may wait on ordinary WASI without
blocking actor dispatch. Policy providers handle immediate bounded decisions
from immutable publications. Authority comes from the enclosing local,
containerized, or remote execution environment, not from a security boundary
between individual plugins.

WASM and extension APIs should operate on workspace, session, pane, command, status, input, and layout concepts. They should not expose Smithay, Winit, EGL, DMA-BUF, or renderer internals as the product API.

The host owns scheduler and product invariants: final commits, worker limits,
completion delivery, input routing, layout validation, pane lifecycle, native
chrome, focus, rendering, and bridges across the session boundary. Validation
keeps host state coherent; it is not presented as containment of malicious
installed plugin code.

See [[systems/presentation-client]], [[systems/plugin-runtime]], [[systems/input-shortcuts]], [[systems/layout-policy]], [[systems/plugin-ui]], [[systems/plugin-graphics]], and [[systems/extension-host]].
