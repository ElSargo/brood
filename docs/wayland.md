# Nested Wayland

Manifold currently hosts nested Wayland clients as managed apps in panes through the v1 `manifoldd` proof of concept.

## Goal

Applications launched inside a Manifold session should create panes through their Wayland windows, be arranged by the presentation surface, and survive presentation window close/reopen where possible.

## Direction

`manifoldd` owns the current nested Wayland compositor state. For the MVP, `manifoldd` also opens the local presentation window and imports/composites hosted Wayland app surfaces itself.

A separate presentation endpoint process is deferred until Manifold needs process isolation or multi-endpoint presentation.

The nested compositor should advertise a stable virtual output as part of the
session runtime, even while no presentation surface is attached. `show` may
resize or render against that output, but clients launched through `enter`
should be able to create windows before any presentation window exists.

The current implementation uses Smithay for the Linux/Rust nested compositor path. That does not make Smithay part of the future WASM or workspace API surface.

## Surface Sharing

The preferred Linux path is DMA-BUF/EGL-compatible surface sharing so client surfaces can remain GPU-resident through composition. Shared-memory and CPU upload paths exist for correctness and limited backends, but they are not the performance target.

## Open Questions

- Which Wayland protocols are required for a useful first version?
- How much window management belongs to Manifold versus the nested client app?
- How should input grabs, popups, drag-and-drop, clipboard, and IME work?
- What is the fallback for unsupported drivers or limited host compositors?
- How should the presentation window handle moving across monitors with different scale, size, and refresh rates while keeping nested clients correctly configured and frame-paced?
- How should Manifold discover and update the presentation output refresh rate? A Shadertoy test in nested Chrome appeared capped at 60 fps even on a 180 Hz monitor. Temporarily advertising a fixed 180 Hz output did not uncap either the hybrid or software Chrome path, so pacing may also be constrained by the host presentation loop, host compositor, or Chrome's Wayland frame-callback/presentation timing behavior.
- Maximize, close, and hide/detach behavior need a focused lifecycle pass. In manual testing, maximizing the presentation window and using close/hide commands did not behave reliably after moving across displays.
