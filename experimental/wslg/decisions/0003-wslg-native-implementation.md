# 0003: WSLg-Native Implementation

## Status

Accepted as the next investigation direction after the v1 nested compositor proof.

## Context

The v1 `manifoldd` prototype proves that Manifold can host native Wayland clients, forward input, detach its presentation window, and reattach without killing the managed app. On normal Linux, the intended high-performance path is a nested compositor that imports client buffers through DMA-BUF with correct synchronization.

WSLg does not currently expose the normal DRM/GBM/DMA-BUF interfaces needed by Manifold's nested Winit/GLES presenter. Under WSLg, GPU-capable clients that could render efficiently when connected directly to WSLg are forced through Manifold's nested compositor. Because Manifold cannot import their GPU buffers on this stack, they fall back to `wl_shm`, adding a CPU pixel path and an extra compositor/presenter scheduling boundary.

## Decision

Treat WSLg as a separate backend implementation, not as a degraded fallback of the generic nested compositor.

The next WSLg investigation should focus on integrating Manifold's pane/session behavior at or near the WSLg compositor layer so managed apps can connect directly to the compositor that owns the WSLg accelerated presentation path.

The implementation options to investigate are:

- a patched WSLg/Weston shell or plugin that adds Manifold-like pane, detach, reattach, lifecycle, and input-routing semantics;
- a sidecar control/data channel where Manifold owns policy while WSLg/Weston owns final surface hosting and presentation;
- a longer-term replacement backend where Manifold speaks the WSLg/RDP/VAIL-facing presentation path directly.

## Consequences

- The current nested Smithay/Winit compositor remains valuable for native Linux and for proving portable session semantics.
- WSLg work should not spend major effort optimizing the current SHM fallback until the WSLg-native feasibility is understood.
- The product abstraction must separate Manifold session semantics from backend-specific surface hosting, buffer import, frame pacing, and output integration.
- Some WSLg work will likely be in Weston/FreeRDP/Mesa/WSLg C/C++ code rather than the Rust `manifoldd` crate.
- WSLg-native testing should compare direct WSLg clients, current nested Manifold clients, and WSLg-integrated clients against the same responsiveness and lifecycle scenarios.

## Open Follow-Up

- Identify the smallest WSLg/Weston hook that can observe and manage toplevel Wayland surfaces.
- Determine whether Weston shell/plugin boundaries are sufficient for Manifold-style detach/reattach without forking large parts of WSLg.
- Map how WSLg's RAIL/VAIL/RDP path handles frame callbacks, damage, buffer lifetime, and monitor refresh.
- Decide how much Manifold policy remains in Rust versus moving into a WSLg-side component.
- Build a private WSLg system distro and prove one custom observable behavior before attempting performance work.
