# Cua divider resize investigation and grind handoff

## Outcome

The production session-switcher/terminal divider currently fails the checked-in
Cua smoothness gate. The final Nix-backed baseline scored **57.16/100 (F)**
across three 1.5 second native Wayland drags with 120 Cua motion steps each.
All three runs reached the requested final divider coordinate and the client
survived this particular sample, but every run incorrectly reused stale
session-switcher content for its final epoch.

Run the gate with:

```sh
nix develop --command bash tools/test-cua-divider-resize.sh
```

The gate requires at least 90/100, no client exit, final geometry within three
pixels, and no stale final content reuse. During diagnosis, retain a failing
run with:

```sh
BROOD_CUA_ALLOW_FAIL=1 \
BROOD_CUA_RESIZE_OUT=/tmp/brood-cua-divider-grade \
nix develop --command bash tools/test-cua-divider-resize.sh
```

The retained `metrics.json` is the machine-readable result, `report.md` is the
short grade card, `frames/` contains sampled visible output, and `client.log`
contains the complete resize/graphics lifecycle.

## What the test measures

The workload is the packaged production topology, not the resize shim:

- the real backend and terminal session actor;
- the real terminal UI, workspace, status bar, and session-switcher actors;
- the external Winit/WGPU client on a private native Wayland compositor;
- the official external Cua Driver on a private daemon socket;
- alternating left/right drags through the session-switcher divider's real
  pointer-policy input zone.

The 100 point grade consists of:

- 50 points for visible divider tracking. Sampled pixels are compared with the
  linear Cua gesture and penalized for lag, stationary samples, and jumps.
- 20 points for protocol efficiency. Superseded synchronized transactions are
  measured against delivered motion events.
- 15 points for correctness: process survival, exact final geometry, retained
  resource validity, and correctly rendered final session-switcher content.
- 15 points for release-to-completed-transaction latency. Full credit is at or
  below 50 ms and reaches zero at 150 ms.

The overall score is the worst run. Intermittent crashes cannot be hidden by
averaging them with successful runs. Process exit, a Cua delivery failure, or
the known referenced-resource failure forces that run to zero.

## Calibrated failing evidence

The baseline artifacts are retained at `/tmp/brood-cua-grade-nix-default` on the
investigating machine. The current implementation produced:

| Run | Motions | Transactions | Superseded | Stale actor frames | Tracking RMSE | Moving samples | Score |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 120 | 121 | 91 | 3 | 0.130 | 42.9% | 57.16 |
| 2 | 120 | 121 | 78 | 6 | 0.145 | 50.0% | 59.67 |
| 3 | 120 | 121 | 80 | 2 | 0.144 | 50.0% | 59.43 |

The visible divider pauses for several samples, jumps to a later transaction,
then pauses again. The low RMSE alone is therefore insufficient; the unique
position and moving-sample terms deliberately penalize this staircase motion.

An earlier 1.8 second Cua drag at roughly 67 motion events per second exited
after only 15 motions with the exact terminal error:

```text
Error: accepted packet lost a referenced resource
```

An immediate repeat survived all 120 motions. The failure is a timing race,
not a deterministic geometry rejection.

## Cause 1: every motion creates a full synchronized transaction

`apply_layout_pointer_message` updates the requested boundary for every motion
and immediately calls `publish_synchronized_pointer_layout`. That call builds
a complete `scale-then-atomic` request whose wait set includes the terminal,
status bar, and session-switcher panes.

The host accepts the new request and aborts the currently active synchronized
resize. It retires the prior epoch, child layout-region work, tagged actor
frames, render tokens, and readiness state before beginning the replacement.
At ordinary 60-120 Hz pointer cadence, the replacement commonly arrives before
the current epoch can collect all three pane-ready responses.

Consequences observed in real runs:

- 121 transactions were created for 120 motions plus release.
- 70-80% of the transactions were superseded under the capture workload.
- terminal PTY resize, terminal redraw, session-switcher text layout, status
  redraw, child-region recomputation, and render scheduling were repeatedly
  performed for geometry that was never presented.
- private atomic layouts leave the previously committed divider visible until
  an epoch promotes, producing stationary intervals and multi-pixel jumps.

The synchronized transaction machinery is useful for committing a coherent
target, but using it as the live pointer animation mechanism makes responsiveness
depend on completing a distributed three-pane barrier inside one pointer-event
interval.

Primary paths:

- `plugins/session-switcher/src/lib.rs`: `apply_layout_pointer_message`,
  `publish_synchronized_pointer_layout`, and
  `publish_synchronized_layout_attempt`.
- `clients/brood-client/src/main.rs`: replacement of
  `active_synchronized_resize` and the `synchronized interaction superseded`
  lifecycle.

## Cause 2: final release reuses content that was never presented

The session-switcher handles `PaneResized` by comparing `state.resize_extent`
with the new target. It then immediately assigns the target to
`state.resize_extent`, before the tagged frame is known to have promoted.

During a drag, the last motion requests width N and records N in
`resize_extent`. That epoch is often superseded. Button release requests the
same width N. `resize_paint_plan` sees equal extents and chooses
`ResizePaintPlan::Reuse`, even though the visible content may still have width
N-1, or may be the original 320 pixel sidebar.

The host correctly accepts reuse of the currently visible scene because reuse
only proves that a pane has visible content owned by the actor. It does not
prove that the content was painted for the target extent. The final layout then
scales stale pixels into the new allocation. The baseline detected this exact
final-epoch reuse on all three runs; the resulting session-switcher text is
visibly stretched and remains so after quiescence.

The actor needs separate requested, painted/staged, and actually presented
extent identities. Equality with the most recently delivered resize event is
not sufficient authority to reuse content.

Primary paths:

- `plugins/session-switcher/src/lib.rs`: `PresentationEvent::PaneResized`,
  `resize_paint_plan`, and `paint_for_resize_or_record_error`.
- `clients/brood-client/src/plugin_graphics.rs`:
  `validate_reuse_pane_content` currently validates actor ownership and active
  epoch, but carries no target-extent correlation.

## Cause 3: accepted actor frames do not retain their resources

The crash is a host resource-lifetime invariant violation.

`TextLayoutCache::queue` considers a replacement accepted when its packet has
been queued into the active actor-frame batch. It then retires layouts unused
by the replacement. `drop_resources` retries only when the host returns
`ResourceInUse`.

The host's `drop_resource` check searches these scene collections:

- currently visible `panes`;
- ordinary `deferred_panes`;
- epoch-tagged `staged_panes`.

It does **not** search `pending_actor_frames` or `deferred_actor_frames`.
Therefore a text layout can be removed while an already accepted frame still
references it. If that frame is subsequently promoted into `panes`, the next
snapshot resolves its packet, fails to find the layout, and returns the fatal
`accepted packet lost a referenced resource` error from
`PluginGraphicsBridge::snapshot`. The top-level client event loop propagates
that error and exits cleanly with a nonzero product failure rather than a Rust
panic.

Primary paths:

- `crates/brood-plugin-sdk/src/graphics.rs`: `TextLayoutCache::queue`,
  `TextLayoutCache::retire`, and `drop_resources`.
- `clients/brood-client/src/plugin_graphics.rs`: `drop_resource`,
  `pending_actor_frames`, `deferred_actor_frames`, actor-frame promotion, and
  `snapshot`.

The immediate invariant repair is to make every accepted or deferred frame a
resource owner for `drop_resource`. Promotion should also revalidate or carry
owned resource snapshots defensively, so an invalid frame cannot become the
visible scene even if a future retirement path misses an owner.

## Why previous proofs passed

The resize-overhaul proof's detailed sixteen-motion lane waits for each motion
to reach a promoted or degraded result before sending the next. Its burst lane
uses a small deterministic burst and proof actors engineered around transaction
timing. It does not combine production session-switcher text resources with a
real terminal at continuous Cua cadence.

The terminal proof stresses rapid compositor/output resizing, which exercises
terminal resize mailbox behavior but not the in-app divider, pointer interaction
chain, session-switcher text cache, or one synchronized transaction per motion.

The session-switcher unit coverage validates individual motion geometry and the
equal-extent reuse decision in isolation. It currently treats equal requested
extents as sufficient for reuse, which is precisely the invalid assumption when
the previous epoch was superseded.

## Recommended grind order

1. Fix resource ownership first. Extend resource-in-use discovery to pending
   and deferred actor frames and add a deterministic unit test that attempts to
   drop a text layout referenced only by each frame state. Add promotion-time
   validation as defense in depth. The Cua gate must never report resource loss
   or client exit, regardless of score.
2. Fix presented-extent correlation. A superseded tagged redraw must not update
   the extent eligible for reuse. The final release at the same requested
   extent must redraw unless content painted for that exact extent is still
   valid and promotable.
3. Separate live preview from durable commit. Update the visible divider from
   the latest pointer coordinate at compositor-frame cadence without launching
   a distributed barrier for every raw motion. Coalesce intermediate terminal
   allocation/redraw work. Submit one exact synchronized durable layout on
   release, or keep at most one transaction active and one latest successor.
4. Remove redundant work revealed by the metrics. Superseded epochs, stale
   actor frames, and stale child-region replies should approach zero. Do not
   improve the score by weakening waits, skipping final terminal correctness,
   or increasing capture tolerance.
5. Run at least the default three Cua drags after every meaningful change. Use
   a larger `BROOD_CUA_DRAG_REPEATS` before declaring the intermittent resource
   crash closed.

## Completion criteria

The resize grind is complete when:

- the default Cua gate scores at least 90/100 repeatedly;
- no run exits the client or reports missing referenced resources;
- all final divider positions are within three pixels;
- the final session-switcher frame is painted for its actual extent, with no
  stale final reuse;
- sampled divider motion is monotonic and tracks Cua without long stationary
  intervals or large catch-up jumps;
- release completion remains at or below the rubric's 50 ms full-credit budget;
- transaction and supersession counts demonstrate coalescing rather than one
  distributed transaction per raw pointer motion.

Do not rewrite a target document to match an intermediate implementation. Any
decision to change the intended atomic resize model belongs in a target-design
change; this report records current evidence, failure mechanics, and the gate
for implementation work.

## Resolution (2026-08-31)

The resize path now passes the Cua gate at **100.00/100 (A+)**. Motion uses the
ordinary layout projection path so the visible divider and scaled pane content
can advance at compositor cadence. Button release submits the single exact
synchronized transaction and performs the durable pane redraw/resize barrier.
The session-switcher no longer treats its last requested extent as proof that
content for that extent was presented; tagged resize notifications redraw even
when the requested extent repeats.

Accepted actor-frame batches now retain every image and text-layout resource
referenced by their pending or deferred scenes. Resource retirement therefore
returns `ResourceInUse` until those batches promote or retire.

The grader records native pointer-event wall timestamps and the start time of
each compositor capture. Tracking is compared only while Cua is actually
delivering the linear gesture. This removes the driver's focus/setup prelude,
post-release captures, and `grim` completion time from the lag measurement
without relaxing the one-motion-step/one-pixel observation tolerance.

Final extended evidence is retained at `/tmp/brood-cua-divider-final-100`:

- six alternating 120-motion drags, all **100.00/100**;
- one synchronized transaction and zero supersessions per drag;
- zero stale actor frames, stale region replies, resource losses, and stale
  final reuse detections;
- exact final geometry in all six runs;
- tracking RMSE from 0.00178 to 0.00447, with 100% unique and forward-moving
  in-gesture samples;
- release completion from 11.221 ms to 28.767 ms.

Verification also passed all 37 session-switcher unit tests and all 281
`brood-client` unit tests through the checked-in Nix development environment.

### 180 Hz follow-up

The default gate now configures and verifies a 180 Hz headless Sway output and,
unless explicitly overridden, derives 270 Cua steps for the 1.5 second drag.
The retained `sway-outputs.json` must report an active mode with refresh
`180000`; a compositor that silently selects another rate fails before grading.

Six strict follow-up runs are retained at
`/tmp/brood-cua-divider-180hz-final-2`. Every run scored **100.00/100** with:

- 270 requested Cua steps and 196 distinct pixel motion events per drag;
- one synchronized release transaction and zero supersessions;
- zero stale actor frames, stale region replies, resource losses, stale final
  reuse detections, or final-position error;
- tracking RMSE from 0.00147 to 0.00329, with 100% unique and forward-moving
  in-gesture samples;
- release completion from 5.675 ms to 15.030 ms.

### Render-geometry invariant follow-up

The client now records a render-time invariant for every pane in the accepted
paint order. It independently compares backing-surface area, projected pane
area, and accepted-layout area, then requires their full pixel rectangles to
agree. Missing surfaces, area mismatches, and geometry mismatches are separate
counts with per-pane diagnostics. The Cua grader treats an absent sample or any
presented mismatch as a hard failure.

The first instrumented run exposed the remaining brief desynchronization: the
terminal and session-switcher surfaces were commonly one pixel behind their
pane and layout rectangles during the drag. Active divider resize now defers
such a render attempt until all three geometries agree, retaining the previous
compositor frame rather than presenting the incoherent projection. The metric
remains global, so startup and non-divider mismatches are still observable;
deferral is limited to the active divider path to avoid blocking the initial
plug-in surface-resize handshake.

Three strict 180 Hz runs are retained at
`/tmp/brood-cua-geometry-metric-3`. Every run scored **100.00/100** with:

- 408 to 410 presented-frame invariant samples per run;
- 1,224 to 1,230 pane comparisons per run, all verified and exact;
- zero missing surfaces, area mismatches, or geometry mismatches in presented
  frames;
- 133 to 152 incoherent intermediate render attempts deferred per run;
- tracking RMSE from 0.00000 to 0.00257, one synchronized release transaction,
  zero supersessions, and exact final geometry.

### Painted-extent and pixel-oracle correction

The allocation-only invariant above was insufficient. A faster 196 px drag in
300 ms, driven with 180 steps and sampled every 10 ms, exposed frames where the
GPU texture allocation, pane rectangle, and layout rectangle agreed while the
packet inside the texture had been painted for an older width. The ordinary
refresh path compiled that stale coordinate space directly into the newer
allocation. Its opaque background ended early, exposing the renderer's clear
color at the right edge. The original grader still awarded 100 because it
observed allocation metadata and divider position, not painted coverage.

GPU canvas preparation now derives an explicit source painted extent from an
opaque origin-anchored canvas fill. Ordinary live previews project that complete
source coordinate space across the current allocation. The same opaque canvas
color initializes the offscreen render target, preventing multisample boundary
coverage from exposing transparency at the outer pixel. Once a production
three-pane frame has established exact geometry, incoherent subsequent frames
are deferred across the complete interaction lifecycle, including release.

The Cua analyzer now checks both projected painted extent and captured pixels.
It scans pane-boundary and output-edge columns for the exact renderer-clear
color; any slice is a hard failure. Regrading the intermediate artifact at
`/tmp/brood-cua-fast-painted-4` finds 23 affected captures across three runs,
including one 12 px slice. The corrected 300 ms stress artifact at
`/tmp/brood-cua-fast-painted-5` has zero clear slices and scores **100.00/100**
in all three runs, with tracking RMSE 0.00248 to 0.00392 and exact final
geometry. The normal 1.5 s, 180 Hz gate also scores **100.00/100** in all three
runs at `/tmp/brood-cua-painted-default-1` with zero clear slices.
