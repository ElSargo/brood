---
status: target
type: system
authority: product
last-reviewed: 2026-09-05
---

# Snapshot Rendering And Input Facets

Plugins publish immutable application snapshots relevant to their panes. The
presentation host selects a published revision and invokes plugin-authored pure
rendering with concrete geometry and target time. Rendering returns concrete
canvas commands and an immutable interaction snapshot describing that layout.
The host commits both together, then invokes plugin-authored input interpretation
against the interaction snapshot associated with the displayed frame.

This is the preferred interactive path. It replaces the earlier target of a
reserved render phase reading the actor's live mutable state. Asynchronous
actor frames remain an explicit migration/compatibility path. One pane
generation has exactly one authoritative production and input path.

## Ownership And Pipeline

```text
actor application state + worker results
                 |
       construct and publish immutable render-view revision
                 |
host selects revision + concrete pane geometry + explicit time/configuration
                 |
       plugin render function
                 |
       draw packets + immutable interaction snapshot
                 |
       validate -> prepare -> commit both with geometry
                 |
platform input + committed interaction snapshot + interaction-sequence state
                 |
       plugin input function
                 |
       semantic actor events + next interaction state + bounded host requests
                 |
       actor validates intent against current application state
```

Actors own application state and ordered semantic decisions. Workers perform
ongoing or expensive work under the actor/worker contract. Render and input
facets are specialized host-invoked plugin roles, not ordinary mailbox turns or
independently running animation workers. Like global key policy, their host
scheduling and restricted capabilities are part of the execution contract.

Plugins retain arbitrary layout, styling, components, hit testing, and event
interpretation in code. Brood owns invocation, pane authority, revision lifetime,
validation, and composition. It does not understand a widget tree or evaluate
plugin layout expressions embedded in draw commands. Transforms and clipping
remain graphics operations; responsive layout is computed by the render facet.

Three kinds of state have separate owners and lifetimes:

| State | Owner and purpose |
| --- | --- |
| Application render view | Actor publishes immutable data needed to render |
| Interaction snapshot | Render result records concrete layout and stable identities |
| Interaction-sequence state | Host retains opaque plugin state for capture, drags, and key sequences |

## Publication

The actor prepares a complete immutable render view and atomically publishes its
revision. Publication replaces one latest root; it does not enqueue snapshots.
Constructing a snapshot may take work. The near-constant-time publication goal
applies to the final root/handle exchange, not to constructing or copying an
arbitrarily large application state.

Unchanged immutable data may be shared between revisions. Terminal rows,
document fragments, and session entries need not all be copied on every update.
No producer may mutate data reachable from a published root, including through
worker aliases. Workers may prepare data, but the actor establishes the coherent
publication boundary and its causal revision. Publication also creates render
interest; ordinary animation demand need not manufacture new application state.

The host acquires and pins a complete revision atomically. A newer publication
does not change an in-flight invocation's inputs. Keep a bounded latest root,
in-flight leases, and committed frame dependencies. Admission, byte and revision
limits, backpressure, and reclamation must prevent a slow renderer or queued
input from retaining an unbounded history. Failed publication leaves the prior
root active. Publication and retirement must not wait for the GPU or reader
completion on a latency-critical thread.

The snapshot is plugin-owned data, not a host schema for application widgets.
The storage/ABI mechanism is deliberately not selected yet. Candidate mechanisms
must prove immutable ownership, safe pinning/reclamation, and bounded transfer
cost before adoption. Raw mutable guest pointers are not a publication protocol.
Sharing within an instance requires a proven ownership discipline; separate
instances require explicit data transfer. Neither parallel execution nor memory
isolation follows merely from calling something a facet.

## Render Contract

Conceptually:

```text
render(pinned application revision, pane targets, target time, configuration)
    -> complete { pane draw outputs, interaction snapshot, next-frame demand }
```

A host-minted invocation token binds client, actor, facet, snapshot, candidate,
pane membership, geometry, host-configuration, and renderer generations. Host
inputs include exact logical/physical extent, scale, clipping, visibility,
focus, negotiated visual parameters, monotonic target time, cutoff, and limits.
The implementation WIT defines exact field syntax after the storage prototype;
this document does not freeze a pointer or serialized-state ABI prematurely.

One invocation covers the relevant owned pane set. Every requested pane returns
one explicit draw or no-change entry; duplicates, omissions, foreign panes, and
invalid resources reject the whole result. No-change requires retained output
whose visual dependencies remain valid, including its interaction snapshot.
Changed geometry requires fresh layout/output in the initial implementation.
Cross-geometry retained-packet replay is deferred until a dependency contract
is specified; rerasterizing an old packet does not recompute responsive layout.

The interaction snapshot contains plugin-defined hit regions, stable action
identities, text-position mappings, or other derived data needed to interpret
input without reconstructing layout. It may retain immutable application-view
references. Draw output and interaction output have one identity and are
accepted, superseded, committed, and retired as a pair. Optional accessibility
metadata follows that same concrete layout revision.

Rendering may allocate bounded scratch space and maintain render-only caches.
It must not mutate published application data, interaction-sequence state, or
any state which changes semantic behavior depending on whether output was
committed, discarded, or rerun. Ambient time, randomness, and mutable environment
reads are unavailable; time and configuration are explicit versioned inputs.

## Input Contract

Conceptually:

```text
interpret(input event, pinned committed interaction snapshot, sequence state)
    -> { semantic actor events, next sequence state, permitted host requests }
```

The host associates input at admission with the frame revision used for routing,
including committed pane geometry and pane-local coordinates. Use the most
accurate platform presentation information available; record commit versus
confirmed-presentation identity honestly when physical display timing is unknown.
Input queued before a frame transition retains its original identity.

The plugin function interprets that frame's layout, not the newest published
application state or a speculative render. Clicking a displayed session yields
its stable session identity even if a newer application revision reordered the
list. The actor receives ordered semantic events with input sequence and source
frame/application revisions, and validates them against current state. A removed
item may cause the actor to reject the intent; it must not activate a different
item now occupying the same coordinates.

Generic picker rows carry caller-supplied stable item IDs, unique within each
picker. Callers preserve these IDs across reorder and replacement updates;
duplicate IDs reject the update atomically. Selection replies retain the item ID
and the displayed value. Positional indices are informational and must not
identify semantic actions. The receiving actor validates the selected identity
against its current domain state before acting.

The host serializes interpretation within an interaction sequence and commits
its returned sequence state and admitted event batch exactly once. Retrying a
pure interpretation must not duplicate actor actions. Reserve bounded delivery
capacity before committing its result; never advance gesture state while silently
losing the corresponding semantic event. Motion may be coalesced only where its
semantics permit it; press/release, text, and composition ordering are preserved.

Capture, drags, and key sequences carry explicit opaque state across frames.
The interpreter reconciles this state with each frame using stable identities.
Host-authorized capture routes subsequent events to the captured pane; the host
still owns platform focus/capture and checks a small negotiated vocabulary of
requests. No arbitrary host commands are allowed. Pane removal, generation
replacement, focus/capture loss, or unavailable historical state cancels the
sequence explicitly. Bounds apply to captured references and queued events.

Keyboard, text, and IME events follow the same ordered route. Application editing
remains actor-owned; the interpreter resolves intended operations, not a second
mutable application model. An interaction-state change may request rendering;
any transient state needed for visuals must enter rendering as an explicit
immutable, revisioned input rather than a hidden mutable read. Such changes
invalidate visual demand just as other host visual inputs do.

## Execution And Capabilities

Plugins are trusted code. Facet isolation aims to contain ordinary plugin
failures and preserve peer progress; it is not a security boundary against
hostile plugins. The same compiled component may be instantiated separately
for actor and facet roles, with forbidden facet imports linked to traps.
Component Model argument lifting can allocate host memory before a dynamic
trap runs. That allocation-amplification risk is accepted and does not require
separate artifacts or a pre-lifting denial adapter. This exception does not
remove normal guest memory/execution limits, snapshot/result admission,
resource accounting, or the requirement to retire failed facet contexts.

Facets execute on bounded host-controlled executors, never Winit or Smithay.
They must not require draining the actor mailbox, waiting for an actor handler,
or freezing arbitrary actor workers to read mutable state. The host can render
the last published view at new geometry while application work is busy.

The concrete runtime must prove this independence. A shared instance is not
acceptable if an active guest call makes the facet unreachable until it returns.
Instance placement, copying versus immutable sharing, and failure scope are
implementation decisions to resolve with measured prototypes before migration.

Render and input imports deny messaging, application publication, worker/process
creation, files, sockets, clipboard, credentials, environment reads, and direct
pane/lifecycle mutation. Semantic events and permitted host requests are returned
as data for host validation. Facets cannot wait for I/O, other guests, or GPU
completion. Fuel/epoch scheduling protects peer progress, while every permitted
host operation also needs a finite bound.

Native text measurement and immutable graphics lookup are permitted. Text/font
configuration is revisioned; pending glyph preparation never blocks guest render
execution. Resource creation uses bounded invocation leases. Acceptance pins
referenced resources, discard releases temporary ownership, and bounded caches
may retain reusable descriptors. Reclamation must not depend on a future actor
input event. No eviction invalidates committed packets or interaction snapshots.

Normal supersession does not interrupt execution. Traps or hard termination do
not imply rollback of caches or safe same-instance retry: invalidate/recreate
the affected facet execution context according to its proven failure scope.
Actor/facet generation replacement cancels dependent output and input authority.
GPU recovery replays retained CPU packets and preserves matching interaction
data; it does not needlessly rerun guest code.

## Scheduling And Presentation

Keep at most one running invocation and one replaceable pending demand per facet.
Run at most once per predicted presentation opportunity. New publications and
host changes coalesce; returned next-frame demand targets a later opportunity.
Hidden panes remain dormant until useful. Ordinary actors, workers, input
interpretation, and peer facets retain progress under continuous animation.

Latest publication selects the next invocation; it does not automatically
invalidate a coherent older result already preparing. Generation invalidation
and incompatible candidate inputs do. Correlate output with the revision it
actually used, never relabel it as the newest application state.

Desired resize geometry is separate from a selected composition candidate.
Pin candidate geometry while attempting preparation so continuous pointer
samples cannot invalidate every render. New desired positions feed the next
candidate. Candidate age and retries are bounded; there is no FIFO of layouts.

An exact resize cohort includes affected visible geometry-sensitive panes and
managed windows. Geometry and required content promote together before cutoff;
a miss repeats the prior complete geometry/content/interaction revision.
Managed windows need their own readiness protocol. Unaffected panes do not block
resize, and state-only updates at committed geometry remain eligible. Topology
changes cancel/rebuild membership; permanently failed members require explicit
recovery or cancellation rather than partial promotion.

Acceptance, graphics readiness, commit, and confirmed presentation are separate
milestones. Keep a complete committed fallback pinned during preparation and
recheck generations at commit. No stale UI texture is stretched to hide a miss.
For an externally resized OS surface, translate/clip the old composition and
fill exposed areas with host background. Hidden panes require valid exact output
before exposure. An initial pane without output uses native loading/recovery UI
and has no plugin interaction authority yet.

## Diagnostics And Acceptance

Trace application input reduction, snapshot construction/publication/acquisition,
render queue/execution, validation, graphics readiness, commit/presentation,
input interpretation, and semantic event admission/reduction separately. Retain
bounded records of revision joins, misses, stale/discarded work, cancellations,
traps, queue depth, packet/snapshot/cache bytes, retained revisions, and losses.
Report p50/p95/p99/max and desired-versus-committed geometry lag.

Implementation proceeds through these gates:

1. Prototype snapshot storage and facet execution. Prove atomic publication,
   immutability, reclamation, bounded retention, and invocation while actor and
   worker workloads remain busy. Measure construction separately from publication
   and any cross-instance transfer; choose the concrete ABI from this evidence.
2. Add render/input exports and denied-import checks with a probe plugin. Prove
   repeat/discard purity, no recursive invocation, peer fairness, ordered input,
   resource cleanup, and generation failure behavior.
3. Migrate the session switcher with concrete geometry and paired interaction
   snapshots. Compare pixels and semantic input with the asynchronous path.
   Click while lists reorder and while speculative renders are discarded.
4. Exercise terminal text, PTY updates, scroll, capture, text/IME, animation, and
   revisioned transient interaction state. Backend workers may retain their
   existing contract; presentation must consume immutable published state.
5. Add cross-actor exact resize cohorts and native-client readiness. A 180 Hz
   divider workload must keep the new-session button within 0.25 physical pixels
   of expected committed geometry. Measure pointer lag separately.
6. Make the path default only after nested workload, replacement, GPU recovery,
   overload, and resource-churn gates pass. Keep asynchronous selection during
   migration, never competing production paths for one pane generation.

Record reference hardware, refresh rate, duration, latency budgets, and maximum
repeated-frame runs before performance acceptance. Infinite repetition must not
pass a geometry-only oracle. Use release builds and the built-in offscreen
nested test CLI through Nix, with screenshots and client/broodd metrics. Do not
freeze the ABI after only a small session list: text-heavy and continuously
invalidated workloads must prove it too.

Related:

- [[plugin-ui]]
- [[plugin-graphics]]
- [[render-scheduling]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[system-profiler]]
- [[../../frictions/resize-element-stability]]
