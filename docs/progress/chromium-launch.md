---
status: current
type: progress
authority: implementation
last-reviewed: 2026-09-10
---

# Chromium launch

The manual Chromium complaint was reproduced with Chromium 147 under private
headless Sway and the real Brood presentation client. The browser loaded its
page and submitted buffers, but its pane did not become visible. The complaint's
Fontconfig, desktop D-Bus, and native-extension warnings were not needed to
reproduce this compositor failure.

Several checks prevented admission:

- At a 1920×1080 presentation, Chromium acknowledged an 800×1046 configure and
  committed that window geometry inside an 820×1066 surface containing shadows.
  The backend compared the surface extent with the configure, rejected the
  valid commit, and left the layout waiting for a snapshot.
- At 1280×720, the workspace allocated a 480-pixel pane against Chromium's
  500-pixel minimum. The backend correctly rejected the exact request, but the
  host-owned layout also rejected its explicitly selected managed recovery.
- A cold first frame could exceed the 250-ms layout deadline. A repeated
  configure at the same size can legitimately receive an acknowledgement
  without a new buffer, leaving repeated exact admission attempts waiting.
- The final paint guard compared managed backing-buffer dimensions with the
  drawn viewport and treated a native loading placeholder as a missing surface.
  It blocked the already selected recovery composition indefinitely.

Managed commit validation now uses Smithay's clamped window geometry, matching
the geometry used by the existing window snapshot renderer. Configure serial,
fresh-buffer, commit-order, and pixel-availability checks remain in place.
Host-owned layouts recover managed panes independently, while still requiring
every affected plugin frame and layout region to be ready. Interactive previews
continue to retain the previous complete composition when managed recovery is
required. Cold managed clients also enter this recovery path at the deadline;
missing exact plugin output still aborts the layout. The paint guard measures
the managed viewport or its native placeholder, matching the renderer's existing
drawing behavior and preserving pane/layout geometry checks. Using global
degraded completion for this case also interfered with the exact plugin
presentation, so that approach was discarded.

The built-in Chromium proof runs both presentation sizes, observes actual page
pixels in client-owned screenshots, sends physical keyboard and pointer input
through Sway, and receives input/click/scroll telemetry from a loopback page.
It retains client and backend logs, Wayland traffic, state and metric snapshots,
screenshots, and a report for each size, and checks window removal on exit.
Profiles and graphical runtime sockets are private and removed afterward.
The proof's entry point is defined in `crates/brood-cli/src/cli.rs`; its workload
and environment overrides are in `tools/test-chromium-nested.sh` and
`tools/test-chromium-nested.py`.

Verification on 2026-09-10:

- The proof against the unchanged package failed at the visible-page check:
  Chromium loaded the fixture, but no page pixels reached the presentation.
- The Nix package build and all 356 presentation-client unit tests passed.
  Rust formatting, shell syntax, Python compilation, and diff whitespace checks
  also passed.
- The fixed package passed the built-in proof at both sizes. At 1920×1080, the
  backend queued the exact 800×1046 snapshot and the layout completed without
  recovery. At 1280×720, minimum-size rejection selected a placeholder, completed
  the layout, and subsequently displayed the browser's ordinary frames.
- Each run delivered 58 keyboard events, four pointer-button events, and 11
  pointer-axis events to the backend. Chromium confirmed the complete typed
  string, button click, and scroll. During input, the backend recorded 18 commits
  and frames at 1920×1080 and 20 at 1280×720, with no DMA-BUF rejections or Wayland
  dispatch errors. Both runs removed the window after browser exit.
- Screenshots were visually checked for typed content and the scrolled page.
  Detailed evidence is in `/tmp/brood-chromium-acceptance`, with the failing
  baseline in `/tmp/brood-chromium-regression-before`.

Related targets: [[../targets/resize-overhaul]] and
[[../targets/systems/plugin-pure-renderer]].

Merge verification on 2026-09-10, including the theme-switcher changes on main:

- The Nix package build, all 359 presentation-client tests, and all 17 CLI tests
  passed. Rust formatting, shell syntax, Python parsing, and diff whitespace
  checks also passed.
- The merged package passed the built-in Chromium proof at both presentation
  sizes, including rendered page pixels, keyboard input, clicks, scrolling, and
  window removal. Input and scrolled screenshots were visually checked.
- Reports, metrics, logs, and screenshots are retained in
  `/tmp/brood-merge-f058-chromium-20260910`.
