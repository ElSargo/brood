# Large diff facet failure and resize freeze — 2026-09-09

The reported live session used the default shell, a terminal, and a diff viewer
stuck at “Refreshing”. Terminal facet work continued, but the failed diff could
no longer supply fresh geometry for exact resize cohorts. The application was
started before the large implementation commit, so the original 19 KiB diff
fixture did not represent its workload.

## Evidence and causes

- The original packaged diff component, given the existing 900 KiB capped diff
  workload in an offscreen real client, traps while `serde_json` decodes the
  entire `DisplayRow` vector: “all fuel consumed by WebAssembly”. The worker
  itself completed successfully. The initial/loading scene could therefore
  remain visible without the application result ever being rendered.
- Facet failure previously set a permanent `failed` flag while leaving that
  generation registered. Later resize cohorts still included its pane and could
  never receive its fresh output. Failure was locally detected but not retired
  through the presentation lifecycle.
- Independently, the live backend had nine component actors and consumed about
  one CPU core. The bounded drain visits eight actors; counting unvisited actors
  as ready backlog caused perpetual self-wakes even with empty queues. A native
  ten-terminal workload reproduced approximately 752,509 idle callbacks/second.
  That workload still loaded the small diff and resized, so the spin alone was
  not accepted as an explanation of the frozen viewer.

Captured evidence is in `plugin-drag-resize-evidence/freeze/`, including the live
client image, selected backend counters, and the baseline facet trap.

## Implementation

Diff publications now contain compact typed row records instead of a JSON copy
of the entire parsed document. The facet scans record metadata and materializes
only viewport rows. Full content height, row origins, line numbers, navigation,
and the existing 900 KiB limit remain intact. The execution budget is unchanged.
Actor viewport dimensions also follow the actual allocation, including panes
smaller than 480×320; narrow headers and footers no longer overlap controls.

All visual facet failure paths queue a generation-scoped retirement. The event
loop ignores stale failures, discards the failed generation’s retained input,
notifies the owning init actor when applicable, and reuses the normal asynchronous
actor/pane removal path. Peer actors and their queued input remain active. The
previous complete composition stays pinned until normal topology replacement;
the geometry guard and exact cohort requirements were not weakened.

Backend rescheduling now counts actual pending observer events rather than the
number of actors omitted from one scheduling quantum.

## Verification

The original failing diff component was injected into a test-only package beside
the repaired host. It still exhausted fuel, was explicitly retired, and the
remaining terminals passed root, tile, and window drags (19–20 sampled positions,
exact release endpoints). This separates failure isolation from fixing the diff.
Artifacts: `/tmp/brood-facet-failure-fixed2`.

The repaired diff loaded a 921,580-byte worker result, navigated Home/End, and
passed all four root/tile/window drags (19–21 sampled positions). End-of-document
and narrow-window captures were visually inspected. Initial verification:
`/tmp/brood-large-diff-fixed`.

Nix-backed workspace tests, diff pure/legacy tests, and workspace Clippy passed:
663 tests in total, one existing ignored test. The diff’s additional 48,000-row
unit case checks deep-tail materialization without constructing the whole row
vector in the facet. No execution limits or performance thresholds were raised.

The normal divider-drag entry point now also runs a large-diff/many-terminal
workload and a terminal-close/reopen workload at fractional scale 1.75. Exact
entry points and environment handling are in `tools/test-plugin-divider-drags.sh`,
`tools/test-plugin-divider-drags.py`, and the reused session-switcher harness.

Final native matrix: `/tmp/brood-freeze-final-native`, all 26 drag cases passed
across the ten visual plugins, large diffs, ten terminals, lifecycle changes,
and scale 1.75. Every plugin's before/during/after contact sheet was visually
inspected, along with full-size large-diff and failure-recovery captures.
The many-actor backend now recorded three callbacks during each one-second idle
sample, versus roughly 752,509 on the original build. The separate injected
failure workload passed four additional drags. Compact results and representative
captures are preserved with the evidence above.

Very narrow fractional-scale allocations still expose text wrapping/clipping in
some plugin chrome and diff columns. The resize freeze is repaired; these
existing compact-layout limitations were not treated as evidence of an exact
geometry or presentation failure.

The unchanged strict rendering/performance gate also passed:
`/tmp/brood-freeze-final-render-gate`. Its report is preserved in the evidence
directory. The verified runtime package is
`/nix/store/xy7i56h0lab8nwshdg9r677p1zz5im61-brood`; `result` points to it.
The live client and backend were inspected without restarting either process;
they need a restart to load the replacement binaries and plugins.
