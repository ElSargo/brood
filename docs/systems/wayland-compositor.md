---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-13
code-links:
  - crates/manifoldd/src/compositor.rs
  - crates/manifoldd/src/state.rs
  - crates/manifoldd/src/winit_presenter.rs
---

# Wayland Compositor

Manifold currently hosts nested Wayland clients as managed apps in panes. The implementation uses Smithay for the Linux/Rust nested compositor path.

The current compositor path can:

- expose a nested Wayland socket
- launch and map hosted Wayland apps
- forward keyboard, pointer, button, and axis input
- preserve hosted apps across presentation detach and reattach
- advertise Wayland globals required by the supported smoke tests
- report frame, input, commit, DMA-BUF, and explicit-sync diagnostics

## Surface Sharing

The preferred Linux path is DMA-BUF/EGL-compatible surface sharing so client surfaces remain GPU-resident through composition. Shared-memory and CPU upload paths exist as correctness fallbacks.

## Presentation

The MVP runtime can attach a Winit/GLES presentation window or an offscreen presentation target. Offscreen presentation exists for nested tests and behavior validation.

The target design removes final viewport composition from the session process. The compositor publishes revisioned window inventory and committed surface-tree frames to demand-driven presentation-client subscriptions. A session presentation lease authorizes one client to request Wayland window sizes and send primary application input. DMA-BUF remains a negotiated local fast path with copied and remote transports as correctness alternatives. See [[presentation-client]].

## Related Pages

- [[../concepts/managed-app]]
- [[../concepts/pane]]
- [[presentation-client]]
- [[testing]]
- [[runtime]]
