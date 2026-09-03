---
status: target
type: system
authority: product
last-reviewed: 2026-09-04
---

# Pure Renderer End-State Specification

Interactive plugin panes may expose a host-driven **render facet**. The facet
reads its actor's live state and turns exact host-owned pane parameters into a
bounded canvas draw packet. The presentation client chooses when to invoke it
and either commits its result with the matching geometry or discards it.
Rendering remains plugin-authored; Brood does not receive or retain a widget
tree.

This is the preferred path for resize-sensitive plugin UI. The asynchronous
actor-frame path in [[plugin-graphics]] remains available for compatibility,
background rendering, and workloads which cannot use the pure contract.

## Status And Scope

This page is the normative end state for the implementation path Brood is
pursuing now. “Must”, “must not”, and “may” describe product requirements even
where the current code has not implemented them.

The specification covers client-local Component Model presentation actors,
their pane-local canvas output, host-driven rendering, and the composition
transaction which makes that output visible. It does not replace the actor
model, move application state into the host, define a host component tree, or
grant plugins direct WGPU or platform-window access.

The completed system has one preferred interactive path and two explicit
alternatives:

| Path | Intended use | Presentation authority |
| --- | --- | --- |
| Same-instance pure render | Normal interactive plugin UI | Host calls the actor export |
| Asynchronous actor frame | Compatibility and naturally asynchronous graphics | Actor answers a render hint |
| Hardened render profile | Future exceptional isolation/parallelism need | Host calls a separate instance from published state |

One pane generation selects exactly one production path. Outputs from two paths
must never race to become authoritative for the same pane.

## Purpose

The actor-frame path couples drawing to actor mailbox and render-hint
scheduling. A fast guest can consequently spend more time waiting before and
after drawing than it spends constructing a frame. It also permits ordinary
target geometry to become visible before the actor frame covering the same
resize event.

The pure path gives the host one operation it can place directly inside a
predicted frame budget:

```text
actor events and worker completions -> live actor state S

latest layout parameters G --+
live actor state S ------------+-> render(G, T) -> validate -> prepare
target presentation time T ----+                    |
                                                   v
                                 atomically present output + G
```

The design separates visual freedom from presentation authority. Plugins keep
their own components, layout, styling, hit testing, and canvas construction;
the host owns invocation timing, geometry, result currency, validation, and
commit.

## Execution Roles

A pure renderer is a reserved host-driven phase of one presentation actor
generation. It executes in that actor's existing Wasm instance and Store, reads
the actor's ordinary live state directly, and may reuse its SDK caches and
graphics resource handles. There is no serialized view-state publication,
duplicate instance, or per-frame state transfer.

The phase is exclusive with actor and worker execution for that generation.
The host waits until no guest call for the generation is active: an actor
dispatch has returned, or worker execution is suspended at a host-controlled
safe boundary. An epoch yield in the middle of an actor handler is not such a
boundary and cannot be reentered. The host may then run a pending render before
dispatching the next ordinary mailbox event. Frontend actors are expected to
keep handlers bounded and delegate blocking or substantial work to workers; a
long handler is therefore a measurable contract violation, not a reason to copy
all state into another renderer by default.

Purity is semantic rather than a claim that linear memory never changes. The
render export may use its allocator, scratch memory, layout caches, memoized
measurements, and render-only SDK bookkeeping. It must not change application
state or produce an externally observable effect whose outcome depends on
whether a render result is committed, discarded, or rerun.

The host enforces the host-call portion of this contract with a render-phase
capability mask. Internal guest-state purity remains a trusted plugin and SDK
contract. Installed plugins are already trusted user code; the base design does
not pay a state-copying cost solely to defend against a plugin violating it.

A future hardened profile may use a separate renderer instance and explicit
immutable state publication for plugins that justify parallelism or rollback.
That is an opt-in escalation with measured costs, not the baseline contract and
not a prerequisite for ordinary frontend actors.

## State And Render Interest

Actor event and worker-completion turns mutate ordinary application state. A
completed turn advances the host-observed reduced event watermark. When the
actor or SDK determines that its visual state changed, it requests rendering;
the host coalesces that interest into one monotonic dirty revision.

An actor may request one render or a time-varying subscription. These express
demand, not permission to invoke the export immediately or recursively. The
presentation client also creates render interest for host-owned geometry,
scale, exposure, theme, and recovery changes without first delivering those
values through the actor mailbox.

One facet runs at most once for one predicted presentation opportunity. Render
interest raised during the export belongs to a later opportunity and cannot
create a same-turn loop. Stop and generation-failure work remains higher
priority; after render deadlines, ordinary actor and worker scheduling retains
peer and same-generation progress.

The render phase captures the actor generation, completed-turn watermark,
dirty revision, and current host parameter revision. Actor state cannot change
concurrently while the phase runs. Events admitted but not yet reduced remain
future state and do not make the captured render internally inconsistent.

## Render Invocation

One host invocation covers the relevant panes owned by a render facet so their
output is internally atomic:

```text
render {
  render-request
} -> render-result
```

The conceptual Component Model surface is:

```wit
record render-pane-target {
    pane: u64,
    geometry-revision: u64,
    logical-width: f32,
    logical-height: f32,
    physical-width: u32,
    physical-height: u32,
    scale: f32,
    visible: bool,
    focused: bool,
}

record pure-render-request {
    token: render-token,
    reduced-watermark: u64,
    dirty-revision: u64,
    target-presentation-time-ns: u64,
    submission-deadline-ns: u64,
    panes: list<render-pane-target>,
}

variant pure-pane-output {
    draw(actor-frame-draw),
    replay-retained-packet(u64),
    no-change,
}

record pure-render-result {
    token: render-token,
    dirty-revision: u64,
    panes: list<tuple<u64, pure-pane-output>>,
    request-next-frame: bool,
}

export render: func(request: pure-render-request)
    -> result<pure-render-result, pure-render-error>;
```

This is a semantic sketch, not frozen field syntax. The checked-in WIT remains
the exact ABI authority once implementation begins. In particular, scale may
become an exact rational and accessibility may use a separately revisioned
result if workload evidence makes either change necessary.

The request includes:

- actor generation, render token, completed-turn watermark, and dirty revision;
- one host geometry revision and the relevant owned pane set;
- exact logical and physical extent, scale, and clip for each pane;
- host-owned visibility, focus, and explicitly negotiated interaction values;
- target presentation time and submission cutoff;
- canvas, text, and output limits negotiated for the generation.

The result includes zero or one packet for each requested pane, the captured
state and geometry revisions, optional accessibility metadata, and optional
continued render interest. Returning no changed packet is an explicit
no-change result for that input, not abandonment.

Geometry is input, never controller state which must first traverse the actor
mailbox. The controller may still receive coalesced resize notifications for
application policy, persistence, or expensive non-render work, but reducing
those notifications is not a prerequisite for drawing the current pane.

Target time is explicit input so deterministic animation does not depend on
invocation latency. Ambient wall time, randomness, environment changes, and
other hidden inputs are unavailable to the facet.

For a geometry-sensitive pane, every new geometry revision must produce a draw
packet for that revision. `no-change` is valid only when neither semantic state
nor geometry changed. `replay-retained-packet` asks the host to lower the
retained logical packet again at the requested target extent; it never means
stretching the prior raster target. The host accepts replay only for a packet
whose declared geometry dependencies permit it. The value carried by the
conceptual replay case identifies the exact previously accepted pane-packet
revision.

## Actor Executor State Machine

The host retains the latest pending render tuple rather than a FIFO:

```text
actor phase:
  idle
  dispatching-event
  running-worker-quantum
  rendering(token, captured tuple)
  stopping

render interest:
  clean
  pending(latest dirty revision, latest geometry revision, target)
```

The required transitions are:

1. An actor event or worker completion may mutate state and request rendering.
   Returning the turn publishes its reduced watermark and leaves one latest
   pending render tuple.
2. A host geometry, scale, theme, exposure, or recovery change may replace that
   tuple without entering the actor mailbox.
3. At the next safe guest boundary, lifecycle failure or stop is handled first.
   A due render may then run before an ordinary queued event.
4. Starting render captures the actor generation, reduced watermark, dirty
   revision, renderer generation, pane targets, and predicted opportunity.
5. No actor handler or worker quantum for that generation runs until the export
   returns or the generation is terminated.
6. A current valid result enters transactional graphics preparation. A stale,
   invalid, or failed result leaves the prior accepted output unchanged.
7. Interest raised during rendering remains pending for a later opportunity;
   the export is never recursively entered.

An actor event admitted before render but not yet reduced is not part of the
captured state. This preserves actor ordering without forcing the host to drain
an arbitrarily busy mailbox before rendering an already coherent state.

The scheduler must give ordinary work progress between presentation
opportunities. Continuous render interest cannot create more than one render
turn per predicted opportunity, and one actor's render cannot consume another
actor's execution budget.

## Render-Phase Imports

While the export is active, host calls which can create application effects are
rejected even though the instance normally has those actor capabilities. The
render phase cannot:

- send messages or commands;
- publish actor state;
- create workers, operations, streams, processes, or timers;
- read files, sockets, environment state, clipboard, or credentials;
- mutate pane, session, focus, layout, or lifecycle state;
- wait for I/O, another guest, GPU execution, or an unbounded host operation.

It may record bounded canvas commands, inspect existing immutable graphics
resources, use native text measurement and layout, and maintain render-only
caches. A render-time create-or-reuse operation must be idempotent for the same
descriptor and bounded independently of whether the resulting frame is
committed. Destructive resource retirement remains an ordinary actor action.

## Latest-Wins Scheduling

Each render facet has bounded state:

```text
running invocation: at most one
pending request: at most one latest replacement
accepted output: one latest complete output per pane
```

New geometry or dirty state replaces pending work. It does not enqueue another
callback. Because actor state is exclusive during the render phase, only
host-owned parameters can supersede a running invocation. When the export
returns, the host compares its actor, dirty, geometry, and renderer generations
with the latest requested tuple. A stale result is recorded and discarded
before graphics acceptance. If interest remains, the latest replacement runs
at the next eligible actor turn boundary.

Normal latest-wins behavior does not interrupt a render. A short stale call
finishes and is discarded. If guest execution cooperatively yields, the
invocation remains the actor generation's active turn while other actors retain
fairness; the presentation frame does not wait past its cutoff. A pathological
render can therefore delay later events for its own actor but cannot block
Winit or unrelated actors.

Hard interruption cannot roll back same-instance memory and is reserved for
generation teardown or failure recovery. A trapped or forcibly terminated
render is not treated as safely rerunnable on the same state. This is the
intentional safety tradeoff which avoids copying normal frontend state.

## Geometry And Presentation Atomicity

A geometry revision is presentable only with exact output from every
geometry-sensitive pane in its required resize cohort. The composition frame
records the geometry revision, actor-state watermarks, render tokens, pane
outputs, and managed-window frames it contains.

If the cohort becomes ready before the cutoff, Brood commits its geometry and
content together. If it misses, the default exact policy repeats the previous
complete geometry/content revision. It does not stretch an old pane texture
under new geometry. This converts insufficient performance into measurable
frame repetition rather than fixed-element distortion.

A plugin may explicitly declare a pane or layer projection-safe. Such content
may use a separately specified fallback, but projection is never inferred from
the existence of an old texture and is not the default for UI panes.

The pointer's latest desired divider position may be retained independently
from the last committed geometry. Diagnostics report their distance and the
reason a candidate revision missed rather than claiming that stale committed
geometry is current.

## Threading And Deadlines

“Synchronous” describes the invocation contract: one request produces one
complete result without later guest callbacks. It does not authorize arbitrary
Wasm execution on Winit's event-loop thread.

The actor executor invokes the facet with host priority at an actor turn
boundary. It starts early enough for measured guest, validation, preparation,
and composition cost to fit the next opportunity. The frame coordinator waits
only until the explicit cutoff, after which it repeats the prior complete
revision and lets latest-wins policy decide whether the result remains useful.

The actor executor is separate from Winit and preserves round-robin progress
for other actors. Render facets remain bounded by wall time, memory, packet,
text, and geometry limits. Hidden or occluded panes do not create an unbounded
invocation stream.

## Validation, Recovery, And Diagnostics

The host copies and validates a complete result transactionally. Packet and
resource rules remain those of [[plugin-graphics]]. Invalid output leaves the
prior complete revision active. Renderer device loss replays accepted CPU-side
packets; it does not rerun guest code unless the accepted state is unavailable.

Diagnostics retain, per invocation:

- actor, dirty, event-watermark, geometry, token, and output revisions;
- requested, started, returned, validated, graphics-ready, composed, submitted,
  and presented times;
- stale discard, deadline miss, long-turn delay, trap, validation failure, and
  repeated-frame outcomes;
- guest wall/CPU time, fuel, packet size, preparation cost, and queue depth;
- desired-versus-committed geometry distance and exact resize-cohort members.

The system profiler joins controller input to completed actor state where
applicable, and independently joins host geometry to pure-render presentation.
It distinguishes waiting for the current actor turn to yield from execution of
the render export and does not silently assign either interval to preparation.

## Incremental Adoption

The pure renderer is an opt-in package capability during migration. A
generation uses one authoritative production path for a pane: pure output and
asynchronous actor-frame output do not race to replace the same pane. Static
resources and accepted canvas packets share the existing host renderer and
recovery machinery.

The first workload is the session switcher. The terminal then tests a
substantially different state shape, text load, and multi-actor resize cohort.
The asynchronous path remains available until those workloads prove the new
contract under normal input, animation, actor replacement, renderer recovery,
and resource churn.

Migration proceeds in independently verifiable stages:

1. Add the render export and actor-executor priority state without changing
   composition. A probe actor proves same-instance state visibility, import
   masking, no re-entry, latest-pending coalescing, and peer fairness.
2. Route the session switcher's ordinary render hints through the host-called
   export while retaining its existing canvas packet construction and text
   resources. Compare packet pixels and input behavior with the asynchronous
   path.
3. Attach exact geometry revisions to pure requests, prepared scenes, and
   composition candidates. Make the new-session-button oracle a required gate.
4. Introduce the cross-actor live-resize cohort so session-switcher and terminal
   outputs become presentable only with their captured geometry.
5. Move terminal rendering to the same path and prove text-heavy state, PTY
   updates, scrolling, and actor-worker interaction.
6. Make pure rendering the default for bundled interactive presentation actors.
   Keep asynchronous production selectable per pane generation and retain a
   rollback switch until repeated real workloads show no regression.

No stage removes the current path before its replacement passes the real nested
workload. Migration does not rewrite draw commands, component SDK code, or WGPU
lowering merely to change who invokes rendering.

## End-State Invariants

- The host is the only authority which starts a pure render invocation.
- The export reads the same live state as the actor and receives host geometry
  directly; no resize event must be reduced first.
- The same actor generation is never executing an event, worker quantum, and
  render export concurrently.
- A render result has exactly one actor generation, reduced watermark, dirty
  revision, renderer generation, and geometry revision tuple.
- Stale work is discardable and never partially replaces accepted pane output.
- Exact UI geometry is never presented with a raster produced for another
  geometry revision.
- Render-phase host effects are rejected, while bounded render-only caches may
  survive discarded calls.
- A missed deadline repeats a complete prior revision and is visible in
  diagnostics; it is not concealed by scaling stale UI.
- Render interest and retained diagnostics are bounded latest-state structures,
  not unbounded callback or packet queues.
- Winit, graphics preparation, and unrelated actors never wait indefinitely for
  one render export.

## Acceptance Before ABI Freeze

The first implementation must prove with a real nested client that:

- a 180 Hz fast divider drag keeps the new-session button's drawn position and
  size within the agreed pixel tolerance for every presented revision;
- session-switcher and terminal content commit with the exact geometry revision
  they rendered, with no whole-pane UI projection;
- rerunning and deliberately discarding the same request has no observable
  semantic effect on actor state or later output, while permitted caches remain
  effective;
- pending requests, packets, caches, and diagnostics stay bounded under a
  faster producer than consumer;
- an intentionally long actor turn delays only its own rendering, is attributed
  separately, and cannot block Winit or unrelated actors;
- actor replacement and GPU recovery cannot present stale-generation output;
- p50, p95, p99, maximum, misses, repeated frames, and instrumentation loss are
  reported for every causal stage.

The ABI is not frozen merely because the session switcher works. At least one
text-heavy pane and one animated or continuously invalidated pane must exercise
the same contract.

Related:

- [[plugin-ui]]
- [[plugin-graphics]]
- [[render-scheduling]]
- [[plugin-runtime]]
- [[system-profiler]]
- [[../../frictions/resize-element-stability]]
- [[../../experiments/host-ui-framework]]
