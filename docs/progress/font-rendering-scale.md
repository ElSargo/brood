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

## 2026-09-09: baselines, terminal grid, and clipped labels

Real-client screenshots reproduced three separate regressions. Cosmic Text's
`LayoutGlyph::physical` scales glyph positions but expects its offset argument
in device pixels. Passing the logical line baseline as that offset moved text
above its intended row at fractional scale, while the terminal cursor stayed
at the correct grid position. The host now scales that baseline offset too.
A regression test checks logical glyph origins and multiline spacing at 1x,
1.25x, 1.5x, 1.75x, and 2x.

The pure terminal also used a fixed nine-pixel column even though the selected
monospace font advances about 9.63 logical pixels. This caused separately styled
runs to overlap and TUI columns to drift. The actor now measures the font on pane
creation and publishes the metrics in its immutable view. PTY sizing, drawing,
cursor and selection geometry, and mouse coordinates all use those metrics.
The terminal's private facet codec advances to version 2 to carry them.

The SDK's single-line clipping helpers previously shaped inside the narrow clip
width, enabling paragraph wrapping before clipping. They now shape at the host's
maximum supported width and apply the caller's clip afterward. This fixes the
bundled picker labels and highlighted-run advances without changing paragraph
layout behavior. This remains bounded by the host's 4096-pixel layout width;
arbitrarily long labels would require explicit no-wrap shaping or bounded
elision. The current bundled labels fit below that bound.

Verification used package `/nix/store/k9l67inw0pb0m32igizadqrbap8ky7nz-brood`:

- Workspace tests: 632 passed; workspace Clippy with warnings denied passed.
  Standalone terminal tests: 26 passed, with Clippy also passing.
- The font mode in `tools/test-session-switcher-plugin-nested.sh` drives a real
  terminal PTY with alternating ANSI styles, Unicode and box drawing, opens the
  same fixture in Helix, resizes Helix, and opens/filters the session picker.
  Both 1x and 1.75x runs passed, with 20 intermediate divider positions and exact
  final geometry in each run.
- Visually inspected the grid, Helix before/after resizing, and picker before/
  after filtering at both scales. Character columns and box borders align,
  cursor rectangles enclose their glyphs, Helix style runs and status rows align,
  and picker labels/highlights remain on one line.
- The full plugin resize matrix passed all 26 cases, including large diffs and
  fractional scaling. Contact sheets were inspected for the other text plugins.
- The standalone real terminal workload also passed styled Unicode, exact paste,
  and subsequent key input checks. Its styled-grid screenshot was inspected.

`result` points to the verified package. Existing client processes must be
reopened to load its host rasterizer and presentation plugins.

Retained before/after images and compact test results are in
[`plugin-drag-resize-evidence/fonts/`](plugin-drag-resize-evidence/fonts/).
The old-package baseline font run stopped at an incorrect picker log predicate
after capturing the terminal/Helix defect. The predicate was corrected to wait
for actual picker visibility before both successful fixed-package runs.
