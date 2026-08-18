---
status: canonical
type: system
authority: product
last-reviewed: 2026-08-01
code-links:
  - crates/broodd/src/compositor.rs
  - crates/broodd/src/runtime.rs
  - crates/broodd/src/control.rs
---

# Wayland Compositor

The session server hosts managed Wayland applications through Smithay. This is
a producer and session-lifecycle boundary, not a presentation endpoint.

The compositor:

- exposes the nested Wayland socket;
- launches and maps hosted applications;
- assigns stable pane identities and surface-session membership;
- processes commits, subsurfaces, popups, and frame callbacks;
- exports one atomic surface-tree frame stream per managed pane;
- reports each managed pane's session membership without using it as a
  visibility filter;
- applies content-size requests and pane-local input authorized by the active
  presentation lease;
- keeps applications and their latest state alive without a connected frontend.

Surface and hosted-process cleanup is driven by their Wayland and child-exit
events. The server does not poll managed-window lifecycle while idle; teardown
repairs focus and inventory on the event that made them stale.

It does not create a Winit window, render an offscreen human presentation,
compute pane placement, perform global viewport hit testing, or run a layout
actor.

## Surface Export

DMA-BUF is the preferred local transport. Shared-memory and copied-image paths
remain correctness fallbacks, and remote transports may encode or send damaged
regions. Transport choice does not change ownership: the server exports
individual pane surfaces and the client performs final composition.

A requested size identifies one managed pane and its logical content extent.
No placement rectangle, z order, clipping rule, or sidebar geometry crosses
the server connection.

## Input

Physical input first enters the presentation client. After client-local key
policy and layout hit testing, managed-pane events cross the bounded input lane
with a pane identity and pane-local coordinates. The server resolves only that
pane's Wayland surface tree and delivers the corresponding seat events. It does
not reject a pane merely because another session is currently selected.

## Diagnostics

Backend diagnostics may expose session inventory, per-window frame production,
surface export, input/configuration acceptance, and protocol errors. Final
layout state, hit-test regions, presentation timing, and composed screenshots
belong to the client.

## Related Pages

- [[presentation-client]]
- [[layout-actor]]
- [[testing]]
- [[runtime]]
