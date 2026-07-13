---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-13
code-links:
  - crates/manifoldd/src/compositor.rs
  - crates/manifoldd/src/geometry.rs
  - crates/manifoldd/src/input.rs
  - crates/manifoldd/src/layout.rs
  - crates/manifoldd/src/presentation.rs
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

Raw display, window, surface, and event-loop handles are confined to the
audited adapter in `winit_presenter.rs`. Its safe-facing functions retain the
typed Winit window, EGL context, GBM device, or calloop source that owns each
handle. EGL construction, foreign Wayland activation, and event pumping cannot
return a raw handle or extend it beyond that owner's lifetime. Smithay display
dispatch has the same shape in `state.rs`: one helper documents that dispatch
preserves the registered display and poll descriptor.

These helpers are renderer implementation boundaries, not plugin APIs. A
plugin can affect visible layout and rendering through the workspace model, but
it never receives a Winit, EGL, Wayland, or calloop handle.

## Debug Buffer Capture

SHM debug capture first validates signed dimensions, offset, stride, visible
row length, total mapped span, and pointer-arithmetic limits. Conversion then
walks complete rows and four-byte pixels through slices. ARGB and XRGB alpha
policy is explicit, and malformed or unsupported mappings produce no image.

Smithay moves a committed buffer into renderer surface state and consumes the
transient compositor `BufferAssignment`. A later DBus debug-dump request cannot
reliably recover that assignment, even while an animated SHM client is
committing. The manifest reports `no_current_buffer` honestly in that case.
The raw-mapping helper is therefore verified with deterministic padded-row,
alpha, short-mapping, zero-dimension/stride, and overflow fixtures. Composed
frame capture remains the stable post-commit graphical artifact.

## Geometry Boundaries

Normalized plugin layout, signed logical layout pixels, unsigned host-window
sizes, and Wayland input values remain distinct domains. Lossy conversions go
through the checked policies in `geometry.rs`:

- normalized layout coordinates round to the nearest pixel; finite and infinite
  overflow saturates instead of wrapping, while invalid `NaN` coordinates map
  to the origin
- positive layout extents retain a one-pixel minimum, but a zero-sized
  presentation remains empty
- unsigned host extents saturate at the signed layout maximum; framebuffer
  capture rejects zero or negative dimensions
- physical pointer coordinates convert through a finite positive host scale and
  clamp to logical presentation bounds; invalid coordinates cannot reach client
  hit-testing
- continuous scroll values must be finite, while wheel-v120 values truncate
  toward zero and saturate finite overflow

Resolved-layout validation remains the policy boundary for plugin-provided
transforms. The conversion layer makes representation changes deterministic; it
does not accept an invalid layout on a plugin's behalf.

## Related Pages

- [[../concepts/managed-app]]
- [[../concepts/pane]]
- [[testing]]
- [[runtime]]
