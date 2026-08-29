---
status: canonical
type: target
authority: product
---

# System Profiler

## Purpose

Brood has one profiler for explaining interactive latency and visual
smoothness across process, actor, WASM, CPU scheduler, graphics preparation,
GPU submission, and presentation boundaries. It retains individual causal
samples. Aggregate counters alone are not sufficient because they erase the
rare paths which determine p99.9 input latency.

The profiler diagnoses; it does not alter scheduling policy. Measurements are
explicitly opt-in, bounded, machine-readable, and usable with a real nested
workload. A capture reports instrumentation loss and cannot claim a reliable
tail percentile when loss is non-zero.

## Clock and identity

Every native process timestamps against the same monotonic kernel clock.
Process, named thread, run, event category, and causal flow identity accompany
each record. A flow ID survives asynchronous handoff and coalescing. One
presented frame may end several input flows, and a superseded render token
transfers its unfinished correlations to its successor.

WASM does not read a separate guest clock for host scheduling analysis. The
host records queue admission, guest dispatch, guest return, and host-command
reduction around the call. Guest-defined spans may enrich that interval but
cannot replace its host-observed boundaries.

## Event model

The format has four primitives:

- spans retain wall time and, when they cannot migrate between threads,
  per-thread CPU time; their difference exposes blocking, descheduling, and
  queue wait;
- causal flows join input receipt to actor dispatch, WASM completion, graphics
  readiness, composition, GPU submission, and presentation;
- instants identify presentation and state-transition boundaries;
- counters describe bounded queue depth, batch size, drops, misses, and
  backpressure.

Records are append-only JSON Lines with an explicit schema version. Conversion
to the Chrome trace-event format is a view, not the source of truth. Offline
analysis computes nearest-rank p50, p95, p99, and p99.9 distributions for
end-to-end flows, each causal hop, frame intervals, span wall time, span CPU
time, and non-CPU wall time.

## Recording behavior

An instrumented binary is inert until a capture directory is configured. Once
enabled, its application threads use a bounded non-blocking channel to a
dedicated writer. Full channels increment a loss counter instead of blocking a
latency-critical owner. The writer periodically flushes. Captures identify
each process independently so a server, presentation client, and supporting
processes may write into one directory.

The profiling Cargo profile preserves optimized product behavior and native
debug information for simultaneous sampling with `perf`, while retaining the
semantic event recorder. Profiling never requires a host-visible presentation;
the built-in nested test path remains the default workload owner.

## Required coverage

The stable coverage surface is:

1. physical input callback and routing;
2. local or network transport admission and server input drain;
3. actor mailbox admission, backlog, dispatch, and return;
4. Component and Preview 1 WASM execution, host calls, and worker completion;
5. graphics lowering and resource upload;
6. redraw wake, frame acquisition, CPU command encoding, and queue submission;
7. GPU completion for submitted work and driver presentation call;
8. observed or explicitly estimated presentation;
9. event-loop callback occupancy, runnable delay evidence, and bounded-queue
   pressure in both presentation and session-server scheduling domains.

Coverage may be added incrementally, but absent boundaries are reported as
missing evidence rather than silently assigned to an adjacent stage.

## Acceptance

A performance claim uses an optimized profiling build, discards an explicit
warm-up interval, and retains enough completed flows for the stated
percentile. p99.9 requires at least 1,000 samples and is preferably evaluated
over several independent runs. The report includes capture loss, maximum
latency, frame-interval tails, target/deadline misses, and CPU versus non-CPU
attribution. A smoothness claim includes both presented-frame cadence and
long-callback evidence; average frames per second alone is insufficient.

The primary optimization loop selects the largest stage contribution among
the worst completed flows, changes one scheduling or implementation cause,
and repeats the same real workload. It does not optimize an average at the
expense of p99.9, actor progress, correctness, or idle behavior.
