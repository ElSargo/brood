---
status: canonical
type: system-map
authority: implementation
last-reviewed: 2026-07-01
code-links:
  - crates/manifoldd/src/lib.rs
  - crates/manifoldd/src/compositor.rs
  - crates/manifoldd/src/control.rs
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
- fast WASM key-handler and map-hook lanes
- async WASM test harness lane

The long-term architecture may split presentation into a separate endpoint process. That split is deferred until the current single-process prototype has proven the behavior contract.

## Data Flow

Hosted Wayland apps connect to Manifold's nested Wayland compositor. Manifold tracks their surfaces as panes in the active session. The presentation endpoint imports or uploads those surfaces and composites the visible session. Closing the presentation window detaches presentation state while hosted apps and session state remain alive.

## Control Flow

User-facing commands such as `enter`, `show`, `hide`, `list`, `switch`, `plug`, and `ctl` resolve a DBus control target. The default target is the entered session when available, otherwise the global Manifold session server. See [[systems/dbus-control]] and [[reference/cli]].

## Extension Boundary

WASM and extension APIs should operate on workspace, session, pane, command, status, and layout concepts. They should not expose Smithay, Winit, EGL, DMA-BUF, or renderer internals as the product API. See [[systems/extension-host]].
