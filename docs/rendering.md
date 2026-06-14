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

## Fallback Policy

Every accelerated surface path must have a correctness fallback:

- browser shared texture -> CPU BGRA upload fallback
- Wayland DMA-BUF import -> shared-memory or CPU upload fallback where practical
- high-refresh animation -> stable non-animated presentation if timing fails

Fallbacks should be visible in diagnostics and logs so performance problems are explainable.
