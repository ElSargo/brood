---
status: progress
type: regression-investigation
authority: implementation
last-updated: 2026-09-09
---

# Real plugin drag resize regressions

**Fixed September 9:** all 18 native drag cases now pass on the actual rebuilt
Nix package, with visual inspection of every plugin. The strict rendering gate
and workspace checks also pass. The original diagnosis and failing evidence
below remain as implementation history; the final repair record follows them.

The September 6 completion checks missed continuous in-client divider behavior.
The new native drag matrix reproduces the reported session-switcher and
diff-viewer failures. The prior passing resize/timing evidence does not establish
that these production interactions work correctly.

Run the matrix through the checked-in environment:

```sh
nix develop --command bash tools/test-plugin-divider-drags.sh
```

The entry point defines package/output configuration. It runs both production
and resize-proof lanes even when the first fails, then generates contact sheets.
The repaired implementation now passes this gate. The test retains the same
progress and exact-geometry requirements; the pure-renderer target is unchanged.

## Workload and assertions

The real packaged brood-client, session backend, and production plugins run on
a private headless Sway compositor. Each drag sends 90 native Wayland pointer
motions at approximately 16 ms intervals without waiting for Brood presentation.
Screenshots sample the gesture about every 80 ms. Before, intermediate, and
after captures remain available, along with client logs and backend status JSON.

All ten visual plugins have coverage: terminal-ui, session-switcher, diff-viewer,
neovim-client, coding-agent, inbox, picker, keybindings, status-bar, and
resize-proof. Seven nonvisual plugins have no canvas to resize and keep their
existing worker/policy/I/O tests. The graphics example is not a bundled plugin.

The production pane dividers resize terminal, diff, editor, agent, switcher, and
inbox content. Picker, keybindings, and status-bar have no user-resizable pane
edge; their cases perform real Super+right-button window-resize drags from
inside the same client, reducing its viewport from 1280x720 to 580x480 and back.
The editor contains a 24-line scratch buffer; the agent contains a wrapping draft
without submitting an external model request. The diff worker reads a populated
project. Resize-proof runs as an additional uniquely named actor in the real
production shell in a separate lane.

The gate requires actual visible extent changes for each intended actor, at
least ten distinct presented boundary positions in the captured gesture, exact
final geometry within three pixels, matching final content/allocation extents,
and no cancelled capture. It retains failures instead of accepting endpoint-only
movement. The reported post-gesture convergence begins after the pointer helper
finishes, not at physical button release; it is not a scanout latency metric.
Presented-position samples come from compositor presentation tracing and are
checked against manually inspected screenshots.

The shared virtual-pointer helper also needed signed interpolation: its previous
unsigned subtraction corrupted negative-direction drags. The corrected helper
is built with warnings as errors and exercised by leftward and rightward native
gestures.

## Session-switcher: impossible child-region readiness

In the first retained real run, both affected pure panes became ready in about
13.4 ms, but the root divider never moved. At 250 ms the host cancelled the
pointer capture before physical release.

The host's synchronized-layout request construction explicitly skips changed
rest-child region discovery for interaction previews
(`clients/brood-client/src/main.rs`, condition before `changed_rest_child`).
Promotion nevertheless always calls `changed_region_chain_is_final`, which
requires that missing changed-child region to exist and be final. Exact pane
readiness therefore cannot make this preview promotable. The timeout cancels
the drag. A deferred motion then carries the unpresented target revision after
rollback, producing `StaleBaseLayoutRevision`.

This is a host invariant mismatch, not evidence that the session-switcher guest
needs more rendering time. Resolving it must preserve exact nested-child
geometry; simply bypassing the final-region check would need separate evidence.

## Diff/workspace: ready epochs superseded before presentation

The first retained diff gestures each delivered all 90 motions and a normal
button release. Each accepted 91 workspace layout transactions, superseded 90,
and completed only the final release transaction. Captures showed only the old
and final widths.

Most intermediate epochs were ready: one direction recorded 83 ready epochs,
the other 90, with mean whole-transaction readiness around 14.3 ms. New motions
arrived roughly every 16.1 ms. Ready work requested redraw, but the next motion
often replaced the active epoch before presentation. Thus ready intermediate
layouts were repeatedly discarded.

Later runs show variable partial progress, including two to nine sampled
positions for the diff and occasional similarly poor editor/resize-proof
tracking. This is consistent with cadence-dependent supersession starvation.
The logs do not establish that full-document guest rendering is the primary
cause; cancelled tokens lack complete guest-duration traces.

## Evidence

Initial diagnosis: `/tmp/brood-plugin-drags-2`.
Full matrix with corrected startup, gesture release, and all ten actors:
`/tmp/brood-plugin-drag-matrix`.
Final scratch-buffer/draft-agent matrix:
`/tmp/brood-plugin-drag-verified`.

Visual inspection caught two editor setup mistakes in intermediate runs: wtype
entered part of an open command as buffer text, and native :e addressed the
local Wasm filesystem rather than the remote project file. The final fixture
uses real keyboard input to populate a scratch buffer directly. Those earlier
runs do not count as populated-file coverage.

This initial matrix exposed the regressions before the repair below.

The matrix is registered as `divider-drags` in the bundled-plugin runner and
coverage inventory, which now audits 17 plugins across 11 lanes.

## Retained final review

The final package is `/nix/store/5v27zj8vcmcy9362zsq66bv19kzxdrg0-brood`.
The last matrix executes 18 gestures across all ten visual plugins. Thirteen
pass the progress/geometry checks; five fail: both root shell gestures, diff
widening, and both resize-proof gestures. Editor, agent, inbox, status-bar,
picker, and keybinding cases pass this run. Terminal moves in the workspace
divider cases but is blocked by the broken root shell preview. Earlier editor
and proof runs demonstrate intermittent progress failures, so a later pass
does not erase that evidence.

All nine contact sheets were manually inspected (terminal is present beside
diff and in the shell sheet), including the editor's populated buffer and the
agent's wrapping draft. Root shell captures remain unchanged throughout the
drag. Diff/proof captures show intermediate stalls and a final jump. Popup
window-resize captures retain readable text and controls as the viewport narrows.

Compact machine-readable results are retained at
[[plugin-drag-resize-evidence/results.json]]. The same directory retains
before/during/after contact sheets for every visual plugin:
shell-narrow.png, diff-wide.png, editor-narrow.png, agent-narrow.png,
inbox-narrow.png, status-window-narrow.png, picker-window-narrow.png,
keybindings-window-narrow.png, and resize-proof-wide.png.

Shell syntax and Python compilation pass. The native workload compiled the
pointer helper with warnings as errors. The bundled inventory audit passes
for 17 plugins and 11 lanes. The regression lane's nonzero result is the expected
product failure, not a claimed successful resize implementation.

## September 9 repair and acceptance

The root preview path now discovers and dispatches changed rest-child regions,
just as release does. It retains the existing final-region and exact-pane
checks instead of bypassing them. The root can therefore complete once all
required content and child geometry are ready.

The workspace actor now retains one pointer resize request until its terminal
notification and coalesces newer desired positions into one queued candidate.
A ready candidate gets its presentation opportunity before the next candidate
is submitted. Release retains its final coordinate. Failure notifications
cannot replay stale same-interaction work or clear another gesture's state.
Rapid second presses use durable visible boundaries rather than unpublished
requested geometry. Unit coverage exercises completion, coalesced release,
failure rollback, stale notifications, and rapid gesture/cancel interleaving.
No WIT or host ABI changed.

Actual package: `/nix/store/4b25bb726k8mmbni23m3a9cb6cp5dh4p-brood`.
Native evidence: `/tmp/brood-drag-fix-native1`. All 18 gestures pass with
19–21 distinct sampled positions, exact final extents, and no cancelled
captures. Shell narrowing completes 91 transactions without supersession.
Diff widening completes 73 transactions without supersession, compared with
the original 91 accepted / 90 superseded / one completed failure.

All nine contact sheets were visually reviewed, covering every visual plugin
(terminal also appears beside the diff and in the shell). Text remains readable,
panes progress through intermediate sizes, and final layouts are coherent.
Fixed contact sheets and compact results are retained under
[[plugin-drag-resize-evidence/fixed/results.json]] and its directory. Earlier
failing evidence remains intact for comparison.

Verification:
- 628 workspace all-target tests pass, with one existing systemd-isolation
  ignore; workspace all-target Clippy passes
  (`/tmp/brood-drag-fix-workspace.log`).
- Six workspace-plugin tests, including the extended transition scenarios,
  pass; its WASM Clippy passes (`/tmp/brood-drag-fix-checks2.log`).
- The strict packaged rendering gate passes with unchanged thresholds
  (`/tmp/brood-drag-fix-render-gate.log`). Its persistent report is
  [[plugin-drag-resize-evidence/fixed/render-scheduling-report.json]].
- The drag report counts anchored host acceptance records, excluding the
  separate accepted-notification log line that previously doubled its
  informational submission counter. This does not change gate thresholds.
