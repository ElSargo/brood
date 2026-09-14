---
status: progress
type: evidence
authority: implementation
---

# Presentation reconnect and actor teardown

On 2026-09-11, two presentation lifecycle races were reproduced and fixed.

Frames arriving before renderer startup were translated from backend pane IDs
to presentation-local handles before being queued. Renderer startup replayed
them through the same translation again. Existing managed windows with sparse
backend IDs could consequently lose their connection to the catalog and input
routes. Pending frames now retain backend IDs until renderer admission. The
regression covers overlapping IDs on two backends, sparse IDs, catalog-first
and frame-first handle allocation, and repeated pending replay.

Actor stop removed the actor from routing before its asynchronous shutdown
completed. The host could still send blur, pointer, key, or geometry events to
its old pane, and buffered actor input could fail on the next flush. Events for
unavailable panes are now harmless. Stop immediately withdraws the panes,
discards their pending creation notifications, and clears that actor's retained
input without disturbing peers. Guest shutdown remains asynchronous.

## Verification

- The reconnect and late-pane-event regressions both failed against the old
  implementation and pass with the fixes.
- All 363 `brood-client` tests pass through Nix. One existing layout conversion
  test expected every supported pointer bit despite supplying only button,
  motion, release, and cancel. Its expectation now checks those supplied bits;
  production mask conversion is unchanged.
- The full `brood` executable builds through Nix.
- The built-in Chromium proof now consumes an unused backend pane ID, replaces
  the presentation twice, checks managed window identity, and drives real
  keyboard, click, and scroll input after reconnect. Its focused reconnect mode
  passed at 1920×1080 with the rebuilt executable and an isolated workspace-only
  configuration: 58 keyboard events, four pointer-button events, 11 axis
  events, and 12 commits/frames in the input interval. Captures show the same
  browser content after both reconnects. The managed browser also closed cleanly.

Evidence is retained beneath
`.codex-artifacts/presentation-lifecycle/chromium-reconnect/1920x1080/`, including
the report, both replacement-client logs, daemon status snapshots, page events,
and presentation captures. `client-tests.log` and `clippy.log` are in the parent
`presentation-lifecycle` directory.

## Verification limits

The full terminal proof stopped before reconnect: the packaged theme-switcher
declaration requested both workspace-read and workspace-write, and the host
rejected that actor; the headless compositor also reported an unavailable
backend DRM FD. The focused Chromium run used a minimal configuration to avoid
that package startup dependency. Its ordinary initial-launch check separately
stalled on tagged resize readiness before pixels appeared. The reconnect-only
mode retains the pixel and input assertions after both replacements but does
not claim initial-launch acceptance. Mode selection and workload setup live in
`tools/test-chromium-nested.py`.

Clippy remains blocked by existing findings in `complaint_capture.rs` and
`themes.rs`; it reports no findings in the modified Rust files. Actor teardown
is covered by the focused runtime regressions; the blocked terminal workload
does not constitute a real terminal-close acceptance run.

## Plugin clicks and workspace focus after reconnect

The default workspace actor treated a nonempty pane inventory as permission to
focus its first pane. The host deliberately reports no subtree focus while the
session switcher or picker owns focus. Echoing an invented workspace focus in
the next layout reply blurred the plugin, cancelling its press before release.
Existing sessions at presentation startup and subsequent session changes expose
this race through their layout publications.

`store_layout_actor_state` now preserves absent focus and rejects focus outside
its pane inventory. Explicit workspace navigation still acquires focus. The
regression exercises focus moving out of the workspace, replacement session
inventories, invalid focus, and explicit navigation. It failed before the fix
(`focus: 11` instead of `null`) and passes with the fix.

The new built-in pointer reconnect proof drives real virtual-pointer input
through offscreen Sway. It checks the switcher's New button, sidebar session
rows, and picker selections with a held button so asynchronous layout replies
arrive between press and release. The unfixed actor reproducibly blurred the
switcher during the first held click. Evidence is under
`.codex-artifacts/pointer-reconnect/before/`.

The proof waits for the shell's composition and the client-rendered session
completion before clicking again: backend activation alone can precede the
frame that re-enables New. Intentional frontend exit is checked separately
from unexpected process failure during input. These readiness corrections
avoid treating disabled controls or expected teardown as input failures.

The fixed actor passed 18 held clicks across 12 session switches and two
presentation reconnects. Each of the three client generations delivered six
plugin button presses and six releases; the daemon recorded no input drops and
retained both managed windows. Captures verify the picker and sidebar after
reconnect. The report, client logs, daemon snapshots, and captures are retained
under `.codex-artifacts/pointer-reconnect/accepted/`. This run used the rebuilt
host and default-workspace WASM with the other packaged plugin components.

Nix-backed checks passed all 12 default-workspace tests, 363 brood-client tests,
and 19 brood-cli tests. The executable build and default-workspace release WASM
Clippy check also passed. Check logs are in
`.codex-artifacts/pointer-reconnect/`.
