---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-01
code-links:
  - crates/manifoldd/src/compositor.rs
  - crates/manifoldd/src/state.rs
  - crates/manifoldd/src/state_services.rs
  - crates/manifoldd/src/winit_presenter.rs
---

# Wayland Compositor

Manifold currently hosts nested Wayland clients as managed apps in panes. The implementation uses Smithay for the Linux/Rust nested compositor path.

Smithay protocol handlers are isolated in `compositor.rs`. Session process
startup, configuration, presentation attachment, event-loop driving, and
ordered shutdown are owned by `runtime.rs`; the built-in nested test topology
and artifact lifecycle are owned independently by `nested_test.rs`. This keeps
test process policy out of Wayland callbacks and gives startup/orchestration
helpers typed launch plans and supervised-process inputs.

The current compositor path can:

- expose a nested Wayland socket
- launch and map hosted Wayland apps
- forward keyboard, pointer, button, and axis input
- preserve hosted apps across presentation detach and reattach
- advertise Wayland globals required by the supported smoke tests
- report frame, input, commit, DMA-BUF, and explicit-sync diagnostics

Session, focus-identity, registry, and test-harness lifecycle facts are owned
by typed services. The compositor coordinates their accepted transitions with
Smithay effects; it does not mutate those catalogues as loose parallel fields.

## Surface Sharing

The preferred Linux path is DMA-BUF/EGL-compatible surface sharing so client surfaces remain GPU-resident through composition. Shared-memory and CPU upload paths exist as correctness fallbacks.

## Presentation

The MVP runtime can attach a Winit/GLES presentation window or an offscreen presentation target. Offscreen presentation exists for nested tests and behavior validation.

## Related Pages

- [[../concepts/managed-app]]
- [[../concepts/pane]]
- [[testing]]
- [[runtime]]
