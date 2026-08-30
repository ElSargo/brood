---
status: active
type: progress
authority: implementation
last-updated: 2026-08-30
---

# Render Scheduling Implementation

Target: [[../targets/systems/render-scheduling]]

This record freezes the acceptance gates for the render-hint, actor-frame
batch, frame-pacing, and executor-isolation implementation. The thresholds are
part of this implementation campaign. They are not lowered to accommodate an
implementation failure. A gate may change only when the harness is shown to
measure the wrong thing, with the evidence recorded here.

All build and behavior commands run through the checked-in Nix environment.
Performance acceptance uses a release build and the built-in isolated `brood
test` path. Debug throughput and a host-visible development window are not
acceptance evidence.

## Code Gates

1. `nix develop --command cargo fmt --all -- --check` passes.
2. `nix develop --command cargo test --workspace --locked` passes.
3. `nix develop --command cargo clippy --workspace --all-targets --locked -- -D warnings`
   passes.
4. `nix develop --command tools/brood-build.sh` builds every host binary and
   every Component Model or Preview 1 guest, validates every component, and
   assembles the default client package.
5. `nix flake check path:.` passes so the result includes untracked source
   during development and exercises the CI-equivalent lint derivation.
6. The guest ABI has one actor-scoped render-interest interface and one
   token-required actor-frame batch submission interface. Bundled plugins do
   not retain an unhinted or pane-at-a-time compatibility submission path.
7. Batch limits are enforced before unbounded allocation or preparation. Pane
   identity, actor generation, token, duplicate pane, packet, resource, and
   resize-epoch validation are all-or-nothing.
8. The Winit and Smithay owners do not invoke guest code, block on actors, wait
   on Tokio tasks, perform backend I/O, or execute unbounded cleanup. Tests or
   assertions exercise this ownership boundary.
9. Presentation actors, graphics preparation, backend actors, and general I/O
   use explicit bounded executors or lanes. No hot-path ordering depends on the
   incidental dequeue order of a general Tokio pool.
10. Every new queue, retained trace, estimate history, and actor scheduler state
    has a release-build bound and generation-scoped teardown.

## Behavior Gates

Focused deterministic tests must prove:

1. Render requests coalesce without losing a request made during an outstanding
   render turn.
2. Enabling a subscription schedules an immediate opportunity; disabling it
   stops continuous opportunities without losing a separate request.
3. An actor has at most one outstanding render token. Armed work has no token
   and can be retargeted; a dispatched token and target are fixed.
4. Every causal actor event at or below the hint watermark is reduced before
   the render hint. Later input causes later render interest.
5. A valid empty batch closes the token as no-change. A rejected batch leaves
   it usable in the same actor turn. Returning without a successful batch
   abandons it, retains dirty state, and enters bounded retry backoff.
6. A deadline miss does not invalidate a live guest turn. A late valid batch is
   accepted for the earliest feasible frame and cannot hold the intended OS
   frame open.
7. Batch acceptance is atomic. Changed visible panes share one actor-frame
   revision and promote together; unchanged panes retain their prior state;
   hidden panes do not become barriers.
8. A newer accepted actor frame can supersede an older unpresented frame without
   either being falsely reported as presented.
9. Actor, pane, client, surface, and renderer replacement cancel or replay state
   without accepting stale tokens or reporting replay as guest rendering.
10. Render-token correlation reaches guest dispatch, batch acceptance, per-pane
    graphics readiness, composition, WGPU submission, and confirmed or
    explicitly estimated presentation.
11. Synchronized-resize epochs remain orthogonal per-pane metadata and preserve
    their existing atomic wait, timeout, supersession, and recovery behavior.
12. Reliable key, button, focus, lifecycle, and render-settlement transitions
    have zero drops. Replaceable motion, damage, and armed render state coalesce
    without an obsolete replay tail.

The Nix-backed nested test must additionally prove the behavior with a real
WASM actor owning at least two visible panes, real keyboard input, Winit/WGPU
composition, Smithay-hosted content, renderer recreation, visibility changes,
and concurrent background actor and I/O load. It must emit a machine-readable
report and finish with `render_scheduling_nested=verified`.

## Performance Gates

The performance lane discards its first 60 presentation opportunities as
warm-up and retains at least 600 measured opportunities per steady-state case.
All percentile gates use the complete retained sample set, not a moving window.

### Deterministic scheduler

With a virtual monotonic clock, fixed 60 Hz, fixed 120 Hz, alternating
8.33/16.67 ms variable-refresh intervals, cost-estimate changes, input
retargeting, and missed targets each run for at least 10,000 opportunities:

- zero duplicate, foreign, reused, or out-of-order tokens are accepted;
- zero request revisions are lost;
- zero stale armed opportunities are dispatched after retargeting;
- every feasible hint is dispatched at the calculated latest-safe instant;
- infeasible work advances exactly one target rather than producing a catch-up
  burst;
- continuously runnable background work makes bounded forward progress.

### Unsaturated 60 Hz nested workload

- key-to-present samples: at least 200;
- key-to-present p50 at most 12 ms;
- key-to-present p95 at most 20 ms;
- key-to-present p99 at most 25 ms;
- key-to-present standard deviation at most 6 ms;
- key-to-present maximum at most 50 ms;
- render-hint deadline-miss rate at most 1%;
- intended-target miss rate at most 2%;
- presented-frame interval p95 at most 20 ms and p99 at most 25 ms;
- zero reliable transition drops, actor failures, renderer failures, or token
  protocol errors.

### Saturated 60 Hz nested workload

The same actor runs while at least three continuously runnable CPU-bound guest
actors, bulk transport traffic, host graphics preparation, and backend I/O are
active:

- key-to-present samples: at least 200;
- key-to-present p95 at most 25 ms;
- key-to-present p99 at most 35 ms;
- key-to-present standard deviation at most 8 ms;
- key-to-present maximum at most 75 ms;
- render-hint deadline-miss rate at most 5%;
- intended-target miss rate at most 5%;
- presented-frame interval p95 at most 25 ms and p99 at most 33.334 ms;
- Winit and Smithay hot-path callback p99 at most 2.5 ms and maximum at most
  10 ms, excluding time inside driver presentation calls which is reported
  separately;
- zero reliable transition drops, actor failures, renderer failures, or token
  protocol errors;
- every background actor completes measurable work during the interval.

### 120 Hz and idle

The isolated direct presentation lane runs a 120 Hz subscription for at least
600 measured opportunities:

- achieved presentation rate is at least 114 frames per second;
- interval p95 is at most 10 ms and p99 at most 16.667 ms;
- intended-target miss rate is at most 3%;
- zero catch-up bursts dispatch more than one outstanding opportunity for the
  actor.

After subscriptions are disabled, requests are satisfied, and presentation has
settled, a five-second idle interval has:

- zero render hints;
- zero actor render turns;
- zero scheduler deadline wakeups;
- zero redraw requests caused by the render scheduler;
- no periodic actor, renderer, or lifecycle polling introduced by this work.

## Regression Gates

1. `nix develop --command cargo run --release -- test render-scheduling` passes
   the new isolated nested lane and writes its metrics and captures.
2. `nix develop --command bash tools/test-terminal-plugin-nested.sh` passes its
   real terminal, Helix scroll, resize, reconnect, teardown, and graphics
   resource assertions.
3. `nix develop --command bash tools/test-resize-overhaul-window-shell-nested.sh`
   passes synchronized resize and slow-provider recovery.
4. `nix develop --command bash tools/test-runtime-plugins-nested.sh` passes
   Component and Preview 1 actor fairness, worker, I/O, and cleanup assertions.
5. The final report contains the code-gate results, behavior invariant counts,
   unsaturated and saturated latency distributions, 60/120 Hz cadence, hot-path
   callback distributions, idle counters, and artifact paths.

## Status

### Final22 acceptance evidence

The immutable final22 release proof passed on its first and only run with exit
status zero. The exact source was
`/nix/store/9lzrg43j4prfhw1s650nwyq0qa0yypg2-brood-render-scheduling-source-final22`
(702 files, `sha256-pWwssFnXGMZa6OQWjqA9JvYnmDHAmaYQR5BI1OzQDfY=`). The
tested package was `/nix/store/aqnvpysiksls8521x37szx4nl002haxj-brood`
(`sha256-D53QxSTxAmo1Ob0kmBuEuAOmBmr5rcYUkq7/WSeYGQ0=`, derivation
`/nix/store/6rw4lp7jjf9bfqawzh29wrg0vr1w3di6-brood.drv`). The complete
machine-readable report is
`/tmp/brood-render-scheduling-proof-final22-20260824/render-scheduling-report.json`;
the artifact directory also contains the raw client, compositor, server, and
deterministic logs plus source, capture, Nix path, derivation, and gate-summary
provenance.

The deterministic suite passed all four 10,000-opportunity cases: fixed 60 Hz,
fixed 120 Hz, alternating variable refresh, and cost-change/input-retarget.
Each case checked 10,000 armed, latest-safe, and catch-up decisions, 1,429
request revisions, four invalid-token attempts, one infeasible-target step, and
background progress. Across all cases there were zero accepted duplicate,
foreign, reused, or out-of-order tokens; zero lost revisions, stale armed
dispatches, latest-safe errors, infeasible-step errors, or catch-up bursts. The
causal-watermark invariant was independently exercised three times with zero
violations.

The real nested workload passed with a WASM component owning two visible panes,
262 real keyboard events, 1,120 atomic multi-pane batches, zero partial actor
frame promotions, one renderer recreation with two retained-state replays,
three visibility changes, Winit/WGPU composition, and Smithay-hosted content.
All 12 behavior invariants in the report were exercised with zero violations.

| Lane | Final22 result |
| --- | --- |
| Unsaturated 60 Hz | 60 warm-up and 1,116 measured opportunities. Key-to-present: 260 samples, p50 10.051 ms, p95 17.272 ms, p99 17.727 ms, standard deviation 4.623 ms, maximum 18.502 ms. Deadline misses 0 (0%); target misses 8 (0.717%). Presented intervals: 1,116 samples, p95 16.592 ms and p99 18.376 ms. Reliable drops, actor failures, renderer failures, and token errors were all zero. |
| Saturated 60 Hz | 60 warm-up and 3,470 measured opportunities. Key-to-present: 261 samples, p50 11.204 ms, p95 18.319 ms, p99 18.999 ms, standard deviation 4.811 ms, maximum 35.809 ms. Deadline misses 8 (0.231%); target misses 19 (0.548%). Presented intervals: 893 samples, p95 16.649 ms and p99 17.767 ms. The three continuously runnable graphics actors completed 903 units each; the backend CPU actors also made progress (114, 116, and 116 units). Reliable drops, actor failures, renderer failures, token errors, and the recorded queue/input/lifecycle/plugin/Wayland failure counters were all zero. |
| Direct 120 Hz | 60 warm-up and 900 measured opportunities at 122.944 frames/s. Target misses 3 (0.333%). Presented intervals: 900 samples, p95 8.455 ms and p99 8.784 ms. Catch-up bursts were zero. |
| Five-second idle | Finalized after 5,539 ms with zero hints, actor render turns, scheduler deadline wakeups, scheduler redraw requests, and periodic actor, renderer, or lifecycle polls. |

The saturated hot-path distributions also passed. Winit retained every one of
39,924 lifetime callback samples, with p99 1.176 ms and maximum 9.611 ms; its
983 separately excluded exact driver calls had p99 0.114 ms and maximum 0.297
ms. Smithay retained every one of 1,984 lifetime callback samples, with p99
0.081 ms and maximum 2.341 ms; its four separately excluded exact driver calls
had p99 and maximum 0.208 ms. The GPU-completion worker recorded 982 exact wait
samples (p99 4.780 ms, maximum 5.141 ms), outside both event-loop callback
domains.

All seven 1280x720 captures passed the automated bilateral/hidden-pane pixel
profiles and fresh visual inspection: the baseline, pre-recovery, recovered,
one-pane-hidden, post-visibility, saturated six-pane, and direct-120-Hz images
under `/tmp/brood-render-scheduling-proof-final22-20260824/captures/`. Their
per-half threshold coverage and distinct-color evidence are retained in the
one-shot proof output; the hidden capture alone has the expected right-half
marker-only profile.

Before the immutable freeze, the harness shell syntax, report assembler and
analyzer byte compilation, analyzer self-test, and diff checks passed. The
independent strict handoff also passed 49 scheduler tests, 233 client tests,
workspace tests and checks, strict all-target Clippy, formatting, analyzer, and
diff validation.

### Earlier implementation milestones

- Terminal scroll proof21 at `/tmp/brood-terminal-fix-proof21` met the pacing
  and input gates (187 changed/presented frames at 62.027 frames/s, p95 16.395
  ms, and exactly 720 pressed plus 720 text events at 239.987 Hz), and its
  complete primary terminal snapshot advanced from input sequence 233 to 953.
  It then exposed a causal-correlation defect: the host queued every complete
  snapshot watermark but assigned only one queued value to each display-paced
  actor frame. Seven hundred twenty snapshots coalesced into roughly 187
  frames, leaving 740 correlations pending even though the final guest draw
  already represented state 953. The replacement contract adds an optional
  per-draw causal state watermark. Terminal UI reports the exact snapshot input
  sequence it encoded; the host bounds that value against the latest complete
  state admitted for the exact pane and carries the reported value unchanged
  through scene readiness and replay. This removes the false one-snapshot per
  frame FIFO without allowing a guest to claim future state.
- Scroll-only proof22 at `/tmp/brood-terminal-fix-proof22` validated that
  replacement end to end: the terminal advanced exactly from state 233 to 953
  and drained all 720 ordinary text correlations in 13 ms, while 186 changed
  scenes equalled 186 presented frames at 61.57 frames/s with p95 interval
  16.386 ms. The first complete follow-up exposed two shutdown-evidence
  defects after the workload itself passed. Ctrl/Alt chords were incorrectly
  admitted as ordinary Text latency samples even though terminal-ui suppresses
  that Text and handles the Key lane separately; the diagnostic now explicitly
  covers the one-to-one ordinary Text lane, retaining Shift and Super-only
  text. The render-scheduling report had also moved graphics sampling before
  actor shutdown and accidentally removed the separate post-shutdown resource
  diagnostic. The client now retains both: pre-shutdown scheduler state and a
  post-runtime-shutdown graphics record proving live resources return to zero.

- The real terminal regression exposed serialized presentation-turn pressure
  under a 240 Hz input burst. Presentation actors can now opt into FIFO-prefix
  batches of at most sixteen small ordinary events; hints, paste, and image
  events stay singleton, completion advances the maximum watermark only after
  aggregate command validation, and each replacement resets to singleton
  delivery. Terminal UI opts in and preserves its existing per-event reducer
  order. This is a coordinated rebuild-only WIT change in the experimental
  0.1.0 world, not a backward-compatibility claim. Focused runtime and terminal
  tests and host/WASM builds pass. The first nested comparison improved the
  scroll workload from 110 frames at 36.1 frames/s (p95 43.39 ms) to 144 frames
  at 47.58 frames/s (p95 33.85 ms), which is material but still below the
  unchanged terminal cadence gate; scheduler re-arm/cost behavior remains under
  investigation.
- A follow-up scroll-only proof at
  `/tmp/brood-terminal-fix-proof19` separated guest submission from host packet
  validation timing, staged bounded producer envelopes, and pre-armed the next
  opportunity after graphics readiness while keeping dispatch blocked until
  composition. It reached 186 frames at 61.488 frames/s with p95 interval
  17.122 ms under 720 host inputs at about 240 Hz. The harness still rejected
  the run because its pre-render-hint requirement of 350 scenes at 115
  scenes/s assumes above-refresh unpaced scene production; actor-frame scenes
  are now display-paced and matched the 186 presented frames. That gate was
  not weakened. Only 206 key correlations had drained by detach, so an explicit
  final acknowledgement/backlog-drain assertion remains required before this
  lane is complete.
- Acceptance gates frozen before implementation dispatch.
- The final11 unsaturated artifact exposed 6,333,580 measured Winit callbacks
  despite only 242 actually-past render deadlines. Winit 0.30.13's Wayland
  loop suppresses empty calloop wakeups only when no timeout is installed, so
  a genuine future `WaitUntil` deadline could spin through empty
  `about_to_wait` turns. The presentation now keeps Winit in event-only
  `Wait` and uses one condition-variable deadline worker to deliver the exact
  earliest render, resize, actor, or benchmark deadline as a generation-keyed
  user event. Deterministic replacement, cancellation, one-shot, and stale
  generation tests passed. A replacement performance proof had not yet run at
  that milestone; final22 above is that replacement proof.
- The non-client workspace passes the all-target Clippy gate with warnings
  denied: `cargo clippy --workspace --exclude brood-client --all-targets
  --locked -- -D warnings`.
- Backend lifecycle request allocation and bounded-lane admission are one
  transaction. Zero or exhausted IDs and a full start lane leave both the ID
  cursor and admission count unchanged. The runnable scheduling-domain suite
  covers success, exhaustion, and full-lane rejection and passes 10/10 tests.
- The plugin SDK focused suite passes 9/9 tests plus its doctest, including the
  maximum 16-pane actor-frame bound and preservation of pending resize metadata
  when a draw is attempted outside an active render hint.
- A workspace `cargo check --workspace --locked` passed. At that milestone,
  full workspace test, full workspace all-target Clippy, nested behavior,
  performance, and regression gates remained acceptance requirements rather
  than being inferred from focused results.
- The immutable final22 behavior and performance gates are accepted. Completion
  regression evidence is recorded below as it is run against the same source.

### 2026-08-29 current-source revalidation

Resumption found that the checkout had moved beyond the immutable final22
source, principally in actor presentation batching, managed-resize recovery,
graphics preparation, and GPU-completion tracking. The current tree passed
formatting, `git diff --check`, shell and Python parser checks, all 277 client
tests and the complete workspace suite, strict all-target Clippy, the complete
host/Component/Preview 1 build, and `nix flake check path:.`. The focused
scheduler evidence remained clean: 10/10 scheduling-domain tests, 52/52
render-scheduler tests including all four 10,000-opportunity simulations, and
18/18 focused component-actor tests.

A release proof against the unmodified resumed tree at
`/tmp/brood-render-scheduling-current-20260829` failed the unchanged latency
gates. Unsaturated key-to-present was 260 samples with p50 23.294 ms, p95
31.004 ms, p99 31.990 ms, and maximum 32.298 ms; 188 of 1,119 measured hints
missed their deadline and 190 missed their intended target. Saturated p50 was
24.969 ms with 660 of 3,477 deadline misses. Frame cadence, direct 120 Hz, idle,
visual, reliability, actor-progress, and renderer-failure evidence remained
healthy, so this is a roughly one-frame interactive-latency regression rather
than general presentation collapse.

The exact immutable final22 package and source were rerun in the same session
at `/tmp/brood-render-scheduling-final22-rerun-20260829` and passed. Its
unsaturated p50 was 9.836 ms with 10 deadline misses; saturated p50 was 11.638
ms with eight deadline misses. This controls for the current machine and
isolated compositor and locates the regression in post-final22 source changes.

The resumed implementation now restores the actor ingress empty-FIFO fast
path: an otherwise-ready event, especially a render hint, enters the bounded
component command lane directly under the ordering lock. Retained FIFO-prefix
batching remains available only after actual backpressure. This removed several
thousand redundant Winit user callbacks in a follow-up workload but did not
remove the extra-frame latency, so it is a useful hot-path correction rather
than the root fix. The deterministic harness now also runs its emitting tests
serially after one parallel run interleaved two JSON records and made valid
runtime evidence unassemblable.

Opting the two-pane proof actor into 16-event guest batches was tested and then
removed: p50 remained approximately 24 ms and deadline misses remained high.
The outstanding investigation is therefore the post-final22 host scheduling
and presentation-boundary path, with particular attention to changes that
removed per-frame GPU-completion wakeups and added immediate post-composition
deadline synchronization. No performance threshold was changed and the current
source is not accepted by the render-scheduling performance gate.

### Profiler-guided latency correction

The systemic profiler localized the resumed regression to scheduling waits:
guest execution and graphics preparation were both sub-millisecond, while a
graphics-ready frame waited roughly one refresh interval for composition. The
scheduler had charged its own planned-wake-to-actual-dispatch barrier delay to
guest execution. It also hid every next opportunity until composition even
after the preceding frame's intended target had passed, so one late frame could
hold the pipeline one cell late indefinitely.

Guest cost is now measured from actual hint dispatch. An uncomposed frame keeps
the barrier through its viable target and a one-millisecond grace period; after
that point the next fixed opportunity can dispatch and supersede the stale
frame through the existing correlation-transfer path. Known barrier delay is
excluded from actor deadline feedback while unrelated event-loop wake lateness
still grows the predictor margin. The focused release scheduler suite passes
all 53 tests, including the four 10,000-opportunity simulations, stale-frame
release, correlation transfer, and visibility atomicity.

A profiler-enabled nested run retained 261 unsaturated input flows with p50
11.527 ms, p95 18.696 ms, and p99 19.751 ms. Graphics-ready-to-composed fell
from 14.533 ms to 1.836 ms p50. The harness's own unsaturated distribution was
11.445/18.675/19.539 ms at p50/p95/p99, down from the resumed tree's
23.294/31.004/31.990 ms. The same run kept partial actor-frame promotions at
zero and measured saturated p50/p95 at 14.794/23.730 ms.

The complete strict proof is not yet accepted. Improved runs still exceeded
the unchanged miss-rate gates (one representative run had 17 of 1,137
unsaturated deadline misses and 21 target misses), and one run recorded a
single protocol error around renderer recovery. A no-profiler comparison kept
unsaturated latency at 10.855/18.150 ms but likewise exceeded the miss-rate
gate. No threshold was relaxed. Per-frame GPU-completion pacing was tested and
removed after it left unsaturated p50 at approximately 25.3 ms and introduced a
roughly 10.5 ms GPU wait.

### Atomic interactive-resize presentation

Interactive layout targets now remain private while their resize epoch is in
flight. Staging no longer changes the visible pane rectangles or dividers and
does not request an intermediate redraw. Epoch-tagged plugin frames are also
prepared without independently requesting presentation. Once the complete
wait set is ready, the renderer installs the target layout, plugin content,
managed content, and dividers in the same Winit promotion frame. When the
cohort misses the current frame, the previous complete presentation remains
visible until that promotion instead of exposing a mixture of generations.

Plugin resize events obtain their target sizes from the staged layout rather
than the still-visible layout. Ordinary resize reconciliation is held while a
staged transaction exists; this prevents an old-size untagged event from
racing and cancelling epoch-tagged guest work. The trace now correlates each
plugin's resize-event enqueue and render-hint dispatch with host-side per-pane
readiness, cohort promotion, and the promoted actor-frame resize epochs.

The release burst proof at `/tmp/brood-resize-smooth-20260830-r4` accepted nine
requests and superseded eight during one drag while keeping at most one staged
layout. All ten pointer acknowledgements were ordered with a measured maximum
round trip of 0 ms. The release promoted both proof panes together, kept the
previous complete divider visible while staged, and finished within one device
pixel of the pointer target without degradation.

The retained window/sidebar proof at
`/tmp/brood-resize-smooth-20260830-r5/window-shell` passed the updated strict
pixel/trace analyzer across 11 native-window captures and 19 sidebar/inbox
captures. A representative sidebar divider epoch waited for five panes:
inbox reuse was ready at 1.878 ms, the fast proof, session switcher, and status
bar at 20.132--21.594 ms, and the deliberately delayed proof pane at 198.770
ms. The visible divider remained at 320 while staged and changed to 272 only in
the single 5/5 promoted frame. Its release epoch repeated the 5/5 atomic
promotion at 195.792 ms.

The focused client suite passes 279 tests, the plugin SDK passes 12 tests plus
its doctest, shell/Python harness parser checks pass, and the complete host and
component build passes. Repeated full nested invocations also exposed a
pre-existing startup-ordering flake: the proof panes can enter inventory after
an empty native-bootstrap transaction has already settled, causing the harness
to reject the run before interactive input. Successful lanes and their strict
analyzers were retained as evidence; that bootstrap race is not treated as an
interactive-resize failure or hidden by weakening the gate.
