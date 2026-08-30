# Scale-aware font rasterization

## 2026-08-30

The presentation client previously asked `cosmic-text` to rasterize every
plugin glyph at scale `1.0`, then enlarged the resulting bitmap when compiling
the logical canvas into a device-scaled GPU target. This made plugin text
inherently blurry above 1x, including fractional output scales.

Glyph shaping and reported metrics remain in logical coordinates. Rasterization
now uses the Winit output scale, and the GPU maps the resulting device-pixel
glyph image back to its logical extent. A scale-factor change also rerasterizes
retained layout handles in the background, so guest-side text caches do not
need to discard otherwise-valid logical layouts merely because the output DPI
changed. The scale-factor event now refreshes plugin render targets and sends
the updated presentation configuration even when the physical window extent
does not change.

Verification:

- `nix develop --command cargo clippy -p brood-client --all-targets -- -D warnings`
- `nix develop --command cargo test -p brood-client` (279 passed)
- The focused glyph regression proves that a 1.5x layout produces a larger
  device-pixel raster than the same logical layout at 1x.

The checked-in nested terminal workload could not run in this environment
because its launcher requires `tmux`, which was unavailable in the Nix shell.
That workload already changes its nested Sway output from 1x to 1.25x and back,
so it remains the appropriate real visual proof once the dependency is present.
Cua Driver also found no running Brood presentation to inspect and could not
reach the desktop AT-SPI bus, so no host-visible fallback proof was recorded.
