# Rendering

The renderer should be a native compositor optimized for Linux and WSL.

## Direction

The current preferred direction is GL/EGL first, because it aligns with:

- Linux and WSL graphics stacks
- browser shared texture and ANGLE/EGL paths
- Wayland DMA-BUF surface sharing
- straightforward CPU texture upload fallback

This is not a final technology decision. Vulkan, WGPU, Skia, or hybrid approaches remain open until targeted spikes prove the tradeoffs.

## Responsibilities

The presentation renderer owns:

- final pane composition
- text shaping and glyph atlas
- image and icon caches
- imported browser and Wayland surfaces
- smooth scrolling and animations
- hit-test metadata for routing input
- frame pacing and damage tracking

The renderer should consume scene commits and surface commits. It should not know Helix commands or own authoritative document state.

## Frame Pacing Goal

The native Linux path should aim for a late-latched one-frame pipeline whenever the client can respond quickly enough:

- receive host input
- forward and flush that input to the nested client immediately
- let the client render and commit a fresh surface
- drain pending nested Wayland commits just before Brood renders
- composite the newest committed client buffer
- submit Brood's frame to the host compositor as late as the frame budget allows

The scheduler should eventually be deadline-driven rather than "render as soon as redraw is requested." It should derive a frame clock from presentation feedback or observed host cadence, reserve a measured render/composite budget plus safety margin, and schedule Brood composition near the end of the target frame.

Diagnostics must make this goal measurable. At minimum they should expose input-to-client-commit, client-commit-to-Brood-submit, input-to-Brood-submit, whether a visible input batch had a matching client commit before the submitted frame, frame cadence, and missed/stale-frame indicators. Winit's current abstraction may not expose enough phase control or presentation timing for the final version; a lower-latency Winit fork or a more direct Smithay/DRM presentation path is explicitly on the table if profiling shows Winit is the limiting layer.

## Fallback Policy

Every accelerated surface path must have a correctness fallback:

- browser shared texture -> CPU BGRA upload fallback
- Wayland DMA-BUF import -> shared-memory or CPU upload fallback where practical
- high-refresh animation -> stable non-animated presentation if timing fails

Fallbacks should be visible in diagnostics and logs so performance problems are explainable.
