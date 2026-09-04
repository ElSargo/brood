---
status: target
type: system
authority: product
last-reviewed: 2026-09-05
---

# Render Hints And Interactive Scheduling

Brood schedules interactive work around the next useful presentation
opportunity. A plugin does not run an independent animation timer and hope that
its result reaches the renderer at the right point. A host-driven pure renderer
is invoked directly with an opaque token, a pinned immutable application snapshot, exact geometry,
and target time. An asynchronous renderer receives the equivalent opportunity
as an actor render hint and returns that token with its visual state.

The same scheduling layer protects the native input and composition paths from
unrelated actor, backend, and I/O work. Winit and Smithay remain the exclusive
owners of their platform event-loop state. They are latency-critical execution
domains, not ordinary tasks submitted to a shared Tokio pool.

This target applies to client-local plugin rendering and to the host work which
turns accepted plugin and managed-window state into a presented OS window. It
does not move plugin policy into the host or make arbitrary guest code run on a
Winit or Smithay event-loop thread.

## Pure Render Opportunities

The preferred interactive path is [[plugin-pure-renderer]]. Atomic application
snapshot publication establishes render demand. The host selects a pinned
revision, concrete geometry, explicit configuration, and target time without
entering the actor mailbox or waiting for actor/worker turns to finish.

There is at most one running invocation and one replaceable pending demand per
facet, with at most one invocation per predicted presentation opportunity.
New publication feeds the next invocation; it does not automatically invalidate
an older coherent result. Desired geometry is coalesced separately from a pinned
composition candidate so sustained input cannot starve every render attempt.
Generation and candidate validity are checked at acceptance and commit.

Render output includes immutable interaction data for its concrete layout.
The host commits geometry, graphics, and interaction identity together. Input
interpretation is bounded host-priority work against the frame associated with
input admission, returning ordered semantic actor events and explicit interaction
sequence state. It does not wait for another render or reconstruct actor layout.

Both facets execute away from Winit/Smithay under bounded scheduling and import
contracts. Publication, interpretation, rendering, and preparation have distinct
budgets and diagnostics. Continuous animation preserves peer and ordinary-work
progress. A missed exact resize cohort repeats the previous complete revision;
input continues to use that revision. Ready work may join an earlier eligible
redraw without violating its captured geometry or explicit time semantics.

The hint/token sections below describe the asynchronous compatibility path.
They do not impose mailbox-watermark draining or actor-turn boundaries on the
snapshot facets. Both paths share presentation prediction and graphics recovery.

## Goals

- time plugin rendering as late as safely possible for the next presentation;
- reduce input-to-visible and animation-frame latency variance, not only mean
  latency;
- correlate every hinted plugin render from dispatch through presentation;
- keep input, Winit composition, and Smithay input/composition responsive under
  actor, transport, backend, and bulk-I/O load;
- preserve bounded queues, peer progress, idle power behavior, and renderer
  recovery.

The scheduler produces a best estimate from available platform feedback. It
cannot promise a literally perfect deadline on an OS or driver which does not
expose reliable presentation timing.

## Render Interest

A presentation actor may express render interest in two forms:

- a **request** asks for one render opportunity because visible state is dirty;
- a **subscription** asks for successive opportunities while an animation or
  other continuously changing visual is active.

Interest is scoped to one presentation-client generation and actor generation.
The actor is the serial execution and state-ownership unit, so Brood does not
pretend that panes owned by the same actor render in parallel. A subscription
can be paused or cancelled without destroying the actor or its panes.

A subscription is demand, not permission to enqueue a stream of callbacks.
There is at most one outstanding hint for an actor. Brood issues the next hint
only after the previous render turn finishes or is abandoned. Hidden, fully
occluded, or detached panes do not keep their actor eligible by themselves. The
subscription remains registered and becomes eligible again when at least one
owned pane can contribute to a presented frame.

Input and other state changes update guest-owned logical state and establish
render interest. For an input-driven change, the scheduler targets the earliest
presentation opportunity which still has enough estimated budget. It does not
delay an already-feasible response merely to align it with a later steady
animation cadence.

### Interest State Machine

The scheduler retains compact state rather than a queue of frame callbacks for
each actor:

```text
subscription: off | on
requested-revision
satisfied-revision
phase: idle | armed | outstanding(token, covered-revision)
```

A render request advances or latches `requested-revision`. Repeated requests
coalesce. A request made while a token is outstanding is newer than that
token's `covered-revision`, so completing the current actor frame cannot lose
the later request.

Enabling a subscription establishes immediate render interest and keeps the
actor eligible after each completed opportunity. Disabling it stops future
continuous opportunities but does not discard a separate pending request or
cancel an outstanding token. A successful batch, including an empty batch,
satisfies the request revision covered by its hint. If a newer request exists
or the subscription remains enabled, the scheduler arms another opportunity.

The scheduler does not enqueue a render hint into an actor's ordinary FIFO
mailbox ahead of later input. It assigns an actor-event watermark and dispatches
the hint only after the actor has reduced every earlier causal input and state
event. Input admitted after guest render execution begins belongs to a later
request. An armed opportunity may be retargeted when new input makes an earlier
presentation feasible; once guest execution begins, its token and target are
fixed.

An actor with no pane which can contribute to a current presentation target
retains its request and subscription state but remains dormant. Actor or client
generation replacement cancels the complete state machine.

## Hint Contract

A render hint conceptually contains:

```text
render-hint {
  token
  target-presentation-time
  submission-deadline
  relevant-panes
}
```

The times use the presentation client's monotonic timeline. The target time is
the instant at which animation state should be sampled. This prevents variable
guest dispatch latency from changing apparent animation speed. The submission
deadline tells the SDK whether unusually expensive work is already at risk of
missing the intended opportunity; it is not an invitation for the actor to
sleep or busy-wait.

The relevant pane set is advisory visibility and damage information for the
actor's current view. It lets the actor avoid unnecessary work but does not
fragment the render opportunity into pane-level scheduling units.

The token is host-minted and opaque. It is bound to the client generation,
actor generation, and one predicted presentation opportunity. It is not a
reusable timestamp or counter chosen by the guest.

The SDK binds the token to one actor frame builder. Canvas commands do not each
copy the token. The actor finishes the opportunity with one batch containing
zero or more changed pane draw packets. Each pane appears at most once. A
synchronized-resize epoch remains per-pane submission metadata and is
orthogonal to the actor render token.

An empty successful batch explicitly settles the token with no visual change.
Batch validation and acceptance are all-or-nothing: an invalid pane, packet,
resource, generation, or duplicate pane rejects the complete batch and leaves
every previously accepted pane state intact. A rejected batch does not consume
the token and may be corrected while it remains valid. The first successful
batch consumes it. Reuse, a foreign scope, or submission after a token has
already settled is rejected.

A batch which arrives after its target deadline may still become the actor's
newest valid visual state. It is recorded as a missed opportunity and does not
cause the compositor to hold the target OS frame open. Every guest actor-frame
batch requires a live render token. Initial pane content and resize responses
request an immediate opportunity. Renderer recovery replays retained CPU-side
state without creating an unhinted guest submission path.

### Token Finality

An armed scheduling opportunity has no token. Brood mints the token only when
guest render execution begins. This lets the scheduler retarget or discard
armed work without manufacturing a stale guest-visible identity.

Once dispatch begins, passing the submission deadline does not invalidate the
token. Guest execution may cooperatively yield and resume as the same actor
turn. A successful batch consumes its submission authority immediately; the
accepted actor-frame revision then carries its correlation through graphics
preparation and presentation. A rejected batch leaves the token usable within
the same turn. Returning from the handler without a successful batch abandons
the token, retains the covered request as dirty, and applies bounded retry
backoff after repeated abandonments. Actor or client generation teardown
cancels it.

Deadline and presentation results are independent diagnostics. `deadline-met`
reports whether the guest submitted by the predicted cutoff.
`target-presented` reports whether the accepted actor frame actually appeared
in the intended OS frame. A late batch can still make the target when the
safety margin was conservative; an on-time batch can miss because later host,
GPU, or platform work overran its estimate.

## Actor Frame Batches

Every successful non-empty batch creates one actor-frame revision. The host
records the token once and retains the individual accepted pane revisions under
that actor frame. Guest render duration is the interval from hint dispatch to
the batch call; validation and graphics-preparation durations remain visible
per pane.

Changed visible panes in one actor frame promote together after all of their
graphics preparation is ready. Until then, composition retains the previous
complete actor frame. If the batch misses the composition cutoff, Brood
presents that previous state instead of mixing actor-frame revisions. Pane
entries which are hidden or belong to no current presentation target retain
the new state for later exposure but do not block visible promotion.

This atomicity is a coherence contract, not a requirement to resubmit unchanged
panes. A batch contains only panes whose retained visual state changed. Panes
omitted from the batch continue to use their prior accepted state and are part
of the resulting complete actor frame.

## Correlation Lifecycle

The token remains attached to the accepted actor-frame batch through lowering
and composition:

```text
render request or subscription
  -> hint token issued
  -> guest dispatch starts
  -> accepted actor-frame batch or no-change
  -> changed pane graphics ready
  -> included in composition frame
  -> WGPU queue submission
  -> OS presentation feedback or best available proxy
```

One OS-window frame may contain new state from several actors, so a render token
is not the composition-frame ID. The composition frame records the set of actor
render tokens, pane revisions, and managed-window frame IDs it includes. This
allows Brood to measure each actor's internal render time while retaining one
global GPU submission and presentation identity.

Superseding an actor frame before composition records the older actor-frame
revision as superseded without reopening its already consumed token. Actor
replacement, loss of every eligible pane, client replacement, renderer
recovery, handler abandonment, and cancellation settle every live token
explicitly. Tokens never survive their owning generation or accumulate as an
unbounded history.

## Frame Prediction

The presentation scheduler predicts the next useful target from the best
available combination of:

- Winit redraw and surface-acquisition behavior;
- OS or graphics presentation feedback;
- observed refresh interval and variable-refresh behavior;
- recent WGPU encode, queue, and presentation durations;
- per-actor dispatch-to-submission duration;
- packet validation, text, geometry, import, and composition preparation;
- a bounded safety margin derived from recent tail behavior.

For each eligible actor, Brood subtracts its total estimated guest render cost
and the relevant host work from the target presentation time, then wakes it at
the latest safe point. Per-pane host preparation remains separately measured,
but serial guest work is not modeled as parallel pane work. Cost estimates are
scoped finely enough that one expensive plugin does not make all plugins render
early. Estimates adapt gradually, retain a conservative cold start, and expand
quickly after a miss.

When precise presentation feedback is unavailable, the scheduler falls back to
a conservative cadence derived from redraw completion and observed refresh.
When a target becomes impossible, it advances to the next useful opportunity
instead of creating a burst of stale hints. Variable refresh, occlusion,
suspend, surface loss, and renderer recreation reset or age predictions rather
than reusing stale timing.

The event loop uses deadline wakeups only while work is pending. A dormant
client with no damage or eligible subscriptions does not poll at the display
refresh rate.

## Scheduling Domains

Brood uses a shared scheduling model but separate executors in each process.
The scheduler records a work item's scope, readiness time, deadline, estimated
cost, and work class. It does not rely on Tokio's incidental task order as a
latency policy.

### Presentation client

The client has these execution domains:

1. **Winit and final composition.** The native event-loop thread owns platform
   input, focus, surface state, WGPU frame acquisition, final command encoding,
   submission, and presentation. Its callbacks perform bounded in-memory work
   and never run plugin code or wait for an actor, transport, file, process, or
   cleanup task.
2. **Presentation actors and facets.** Bounded execution domains run client-local
   actors and host-invoked render/input facets. Facet admission and execution
   remain independent of busy actor/worker calls under [[plugin-pure-renderer]]. Render hints and input-caused actor turns carry deadlines and cannot
   wait behind unrelated backend or bulk work. One actor generation still runs
   at most one turn at a time, and Wasmtime yield points enforce peer progress.
3. **Graphics preparation.** Bounded workers perform parallelizable text,
   geometry, image, and pipeline preparation. Results are immutable,
   generation-stamped inputs to the Winit owner; workers never mutate live
   compositor state.
4. **Client I/O and services.** Transport, clipboard, deployment, filesystem,
   process, and cleanup work uses general asynchronous or blocking executors.
   It communicates with the interactive domains through bounded semantic
   lanes.

### Session backend

The server has corresponding but independent domains:

1. **Smithay and hosted input/composition.** The calloop owner handles Wayland
   protocol state, seat and input forwarding, commits, damage, frame callbacks,
   and managed-window composition/export. It does not run backend actors or
   block on general Tokio work.
2. **Backend actors.** A dedicated bounded executor runs daemon and session
   actor turns, with reserved admission for lifecycle, cancellation, input
   consequences, and compositor-required completions.
3. **Backend I/O and services.** Network, storage, subprocess, D-Bus, artifact,
   and bulk work runs outside the Smithay owner and actor executor.

Winit and Smithay may require one owner thread rather than a pool. Separation
means exclusive ownership and reserved runnable capacity, not parallel access
to platform objects. Platform-specific affinity, quality-of-service, or thread
priority may improve this boundary, but correctness and bounded latency cannot
depend on privileged real-time scheduling.

## Work Classes And Fairness

Scheduling distinguishes semantic work classes:

- platform input and the bounded work required to submit an imminent frame;
- deadline-bearing plugin render and input-response turns;
- lifecycle, cancellation, focus, and ordinary actor transitions;
- replaceable latest-state work;
- background service and bulk work.

The platform owner always drains the bounded critical work necessary to keep
input and presentation coherent. Deadline-bearing actor work is selected by
deadline within its executor. Estimated-cost admission prevents starting work
which cannot finish before the target when a later frame is the honest choice.
Replaceable state is coalesced before execution.

Priority does not mean starvation. Each executor has bounded per-turn work,
reserved capacity for lifecycle and cancellation, and an aging or fair-share
rule for continuously runnable non-interactive work. A plugin which repeatedly
overruns its estimate has its future hint lead time expanded and may be
throttled to fewer opportunities; it cannot occupy the Winit or Smithay owner.

Cross-domain queues retain their semantic rules. Keys, buttons, focus, and
lifecycle are reliable bounded transitions. Pointer motion, damage, and other
replaceable samples are latest state. A saturated background lane cannot own
the only wakeup or permit needed by an interactive lane.

## Renderer And Recovery Rules

Brood never waits for every hinted plugin before presenting. At the composition
cutoff it uses each pane's newest graphics-ready accepted state and records
which hints missed. Synchronized resize remains the explicit exception: its
fixed wait set and timeout rules continue to govern atomic promotion.

Surface loss, device loss, and renderer replacement replay accepted CPU-side
state without asking dormant actors to redraw. Outstanding hints are cancelled
or retargeted with fresh tokens. A replay is identified as replay, not falsely
reported as a new plugin render.

## Observability

Correlated traces retain input sequence where applicable, render token, actor
and pane generation, accepted draw sequence, composition frame, WGPU
submission, and presentation identity. They measure at least:

- interest-to-hint and hint scheduling error;
- hint dispatch-to-guest-start and guest-start-to-submit;
- validation and graphics-preparation duration;
- ready-to-composition and composition-to-queue duration;
- queue-to-presentation duration or the available proxy;
- target error, deadline misses, expiry, no-change, cancellation, and
  supersession;
- runnable delay and queue depth by execution domain and work class;
- key-to-present and frame-interval p50, p95, p99, maximum, and variance.

Instrumentation has bounded retention and does not introduce a global lock on
the hot path.

## Verification

The design is accepted only with Nix-backed real workloads which exercise the
actual Winit, WGPU, actor, transport, and Smithay paths. Verification includes:

- a continuously animated client-local plugin at fixed and variable target
  cadences;
- terminal typing and scrolling with correlated input, hint, draw, and present
  identities;
- simultaneous CPU-bound actors, backend work, transport traffic, and graphics
  preparation which cannot starve input or composition;
- an intentionally slow plugin which misses hints without delaying unrelated
  panes or the OS window;
- hidden, exposed, suspended, and replacement-client subscription behavior;
- renderer recreation and synchronized resize with outstanding hints;
- idle measurement proving that no pending render interest means no refresh
  polling.

Reports compare latency distributions and frame-interval variance before and
after the scheduler. Compile-only, isolated queue, or synthetic scheduler tests
are necessary lower layers but are not sufficient acceptance.

Related:

- [[plugin-pure-renderer]]
- [[plugin-graphics]]
- [[plugin-runtime]]
- [[presentation-client]]
- [[ui-concurrency]]
- [[presentation-transport]]
- [[input-shortcuts]]
