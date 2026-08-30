---
status: progress
type: evidence
authority: verification
last-reviewed: 2026-08-29
---

# System Profiler

## Implemented foundation

The `brood-profiler` crate now owns the shared monotonic event format, bounded
writer, causal flow API, scoped macros, offline percentile/stage report, and
Chrome/Perfetto conversion. The workspace has an optimized `profiling` Cargo
profile with native debug information.

The presentation render scheduler emits one causal flow per correlated key.
It follows correlation transfer through superseded frames and records actor
dispatch, accepted WASM frame, graphics readiness, composition, GPU
submission, and presentation. Frame cadence is recorded separately. Winit
callbacks, CPU render/present work, graphics preparation, GPU completion waits,
Component actor dispatch, server runtime ticks, backend actor drains, and the
server input fast-path drain emit wall/CPU spans or counters.

## Current constraint

This foundation records GPU queue completion but does not yet issue per-pass
hardware timestamp queries. Queue completion is sufficient to separate CPU
submission from completion pressure, but a driver-specific shader/pass stall
still requires WGPU timestamp-query enrichment or an external GPU tool. Native
input correlation is complete for presentation-local actor rendering; managed
application input still needs a protocol correlation field before one flow can
cross the client/server/Wayland application boundary without sequence
inference. These omissions are recorded in the corresponding friction note.

## Real workload evidence

The optimized `profiling` build completed. A real offscreen render-scheduling
run produced 60,146 semantic events across three presentation clients and
three server processes with zero recorder-channel drops. It retained 3,590
complete flows, including 2,790 mailbox flows and 787 complete key-to-present
flows. One server was terminated by the harness with a partial final JSONL
record; the analyzer now reports and skips only a malformed final record while
continuing to reject corruption in the middle of a stream.

The profile attributed the 20.525 ms key-to-present median primarily to two
waits: key receipt to actor dispatch (6.523 ms median, 17.004 ms p99) and
graphics readiness to composition (14.252 ms median, 15.456 ms p99). Actual
actor dispatch to accepted frame was 0.175 ms median, graphics preparation to
ready was 0.364 ms, composition to GPU submission was 0.172 ms, and the driver
present call boundary was 0.006 ms. Component mailbox enqueue-to-dispatch p99
was 0.018 ms and dispatch-to-return p99 was 0.055 ms over 2,790 samples. The
data therefore localizes the current tail to frame-opportunity scheduling, not
WASM execution, mailbox contention, graphics lowering, or the GPU submission
call.

The existing render-scheduling acceptance report failed its unchanged
unsaturated p50 gate. This is consistent with the already recorded roughly
one-frame regression in [[render-scheduling]], and the new stage attribution
narrows it further. The run was not a clean performance acceptance result:
only 787 input flows were retained, below the 1,000 samples required for a
p99.9 claim, and a temporary package assembled for the untracked working tree
contained stale unrelated terminal components. The graphics proof actor,
visual captures, saturated lane, and direct 120 Hz lane nevertheless completed
far enough to provide the profiler evidence above.

## First scheduling correction

A follow-up profile used the causal stages to identify two scheduler-accounting
errors around the composition barrier. Guest cost included the time between a
planned wake and actual dispatch even when the scheduler itself deliberately
held the hint behind an uncomposed frame. A frame which had already missed its
target could then keep that barrier indefinitely, preserving the one-frame-late
steady state. The scheduler now measures guest execution from actual dispatch
and releases only a stale composition barrier, one millisecond after its
intended target; a still-viable frame retains atomic protection.

In the real unsaturated two-pane workload, the resulting profile retained 261
complete input flows with zero recorder drops. Input-to-present fell from
23.671 ms p50 in the comparison capture to 11.527 ms p50, 18.696 ms p95, and
19.751 ms p99. Graphics-ready-to-composed, previously the dominant 14.533 ms
median wait, fell to 1.836 ms. Actor dispatch-to-WASM acceptance remained
0.311 ms p50 and accepted-to-graphics-ready remained 0.567 ms, confirming that
the improvement came from opportunity scheduling rather than faster guest or
graphics work.

Restoring a GPU completion wait for every frame was also measured and removed:
it raised GPU completion wait to 10.545 ms p50 while leaving input at 25.310 ms
p50. That rejects GPU queue completion as the pacing source. Strict
render-scheduling acceptance is still pending because the improved runs exceed
the existing deadline/target miss-rate gates even though their latency
percentiles now meet the unsaturated thresholds.
