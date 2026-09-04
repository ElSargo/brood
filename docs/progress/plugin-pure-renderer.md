---
status: active
type: progress
authority: implementation
last-updated: 2026-09-05
---

# Snapshot Facet Architecture Review

Target: [[../targets/systems/plugin-pure-renderer]]

The architecture review selected immutable application-view publication followed
by host-invoked render and input facets. This supersedes the same-instance
live-mutable-state render phase proposed earlier. No runtime implementation or
ABI migration is claimed by this documentation change.

The actor publishes a coherent view; render produces concrete draw commands and
an immutable interaction snapshot; the host commits them together. Input uses
that frame's retained interaction data plus explicit sequence state and returns
semantic actor events. Layout remains arbitrary plugin code, not deferred host
layout expressions in packets.

Code review found actual actor/worker shared-state usage in terminal-session,
neovim-client, neovim-files, and coding-agent. Session-switcher has a no-op worker;
terminal-ui workers wait on timers and return payloads. Existing worker sharing
is not removed by the chosen target. Published views must not expose mutable
aliases, and facets must not depend on freezing those workers.

Next work is a storage/executor prototype, not immediate product migration.
Measure snapshot preparation, atomic publication, acquisition, transfer, retained
bytes, and invocation under busy actor/worker load. Resolve ABI representation,
reclamation, isolation/failure scope, and input admission capacity before freezing
interfaces. Near-instant publication is a root-exchange goal, not evidence that
snapshot construction is free.

Verification for this revision: documentation consistency searches and
`git diff --check`; no build or behavior tests, because no runtime code changed.
Existing historical progress records and alternative experiments remain history,
not updated claims that this target has already shipped.

## Implementation Campaign And Review Gates

The implementation is overseen against the target above. Each milestone must
record changed code, verification evidence, remaining limitations, and a review
outcome here. Completing a milestone does not imply the later milestones pass.
Target changes require an intentional design decision; implementation obstacles
belong in progress/friction records.

### M1: Publication And Execution Prototype

Start from `crates/brood-wasm/src/policy_provider.rs`,
`global_key_policy.rs`, and `component_actor.rs`, plus the Component Model WIT.
The existing policy registry has immutable `Arc` leases, but `append_frame`
constructs a byte allocation and retains a predecessor chain. Its fixed chain
depth does not bound all externally pinned frames. Global-key invocation copies
publication bytes into its separate guest memory. These are reuse candidates,
not proof of the render publication/transfer budget.

Deliver a runnable prototype comparing a bounded copied immutable view with a
revisioned immutable-chunk/handle approach where practical. Separate preparation,
root exchange, acquisition, guest transfer/read, and final reclamation metrics.
Exercise a small session-list shape and a changing terminal-row shape. Account
for every retained allocation, including superseded views pinned by input or
rendering. Overload must reject/defer publication without corrupting the latest
accepted root. Teardown must invalidate authority even while memory leases exist.

Run a guest facet while its associated actor and worker are deliberately busy.
The proof must execute guest code through the intended runtime; a host-only
queue or Arc test cannot establish Wasmtime execution independence. Verify denied
imports, bounded execution, and peer progress. Explicitly choose instance and
storage placement only after reviewing these results. Do not migrate a product
pane or freeze the ABI in this milestone.

Review gate: coherent immutable views, bounded total retention, independent
invocation, measured transfer costs, and a documented failure/reclamation model.
Any missing item prevents M2 ABI selection.

### M2: Paired Render And Input Protocol

Implement the selected publication mechanism, optional facet exports, generation
and candidate identities, and paired draw/interaction leases. Add ordered input
interpretation with explicit gesture state and reserved semantic-event capacity.
Reject speculative interaction promotion, duplicate event admission, and stale
pane/generation authority. Resource retirement must run without waiting for an
ordinary input event. Prove retry/discard purity and failure behavior using a
probe plugin before product integration.

Review gate: an event is interpreted with its admitted frame revision; state and
semantic delivery commit together exactly once; discard cannot alter visible
interaction; traps cannot silently reuse an unsafe execution context.

### M3: Session Switcher Vertical Slice

Separate switcher application publication from size-dependent layout. Reuse its
canvas/text construction where compatible, returning opaque interaction data
with each render. Feed semantic actions back through actor ordering. Compare
normal input and pixels against the existing path; exercise reordered/deleted
sessions, resize, discarded candidates, focus, and capture transitions.

Review gate: a real nested client passes screenshots and semantic input checks,
and traces demonstrate the path no longer waits for actor render hints. Keep
rollback selection per pane generation, never competing authoritative outputs.

### M4: Terminal And Continuous Interaction

Publish complete terminal render views, reusing immutable rows where the selected
mechanism supports it. Preserve backend PTY/worker ownership. Exercise output
bursts, typing, scrolling, selection, text/IME, blink, and simultaneous resizing.
Measure retained rows/text resources and input delivery under overload.

Review gate: text-heavy and continuously changing output meet the same ownership,
ordering, resource, and execution contracts as the switcher.

### M5: Exact Composition And Recovery

Integrate cross-actor geometry candidates, paired graphics/interaction commit,
native-window readiness, and pinned complete fallback output. Exercise generation
replacement, failed cohort members, hidden/exposed panes, and GPU recreation.
Fix reference hardware and latency/repetition budgets before performance grading.
Use the 0.25 physical-pixel committed-geometry oracle together with pointer lag
and repetition metrics; geometrically stable frozen output is not success.

Review gate: release-mode nested workloads with screenshots and client/broodd
causal metrics pass. Only then default bundled interactive panes to facets.

### Verification Discipline

Use Nix for Cargo and the built-in offscreen nested test entry point. Run focused
meaningful checks after each change and repository-required checks at integration
boundaries. No host-visible development harness for automated acceptance.
Compilation, synthetic tests, and profiler instrumentation are supporting
checks; real workload evidence is required for product migration. Report failures
and unresolved design tensions rather than weakening a gate to fit the code.

Current status: campaign established; M1 investigation begun. No prototype,
runtime change, or behavior acceptance has yet been completed.
