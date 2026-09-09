---
status: active
type: progress
authority: implementation
last-updated: 2026-09-05
---

# Snapshot Facet Implementation Campaign

Target: [[../targets/systems/plugin-pure-renderer]]

## Initial Architecture Review

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

Current status: M1 prototypes are implemented and undergoing integration review.
Product WIT, presentation authority, and bundled plugin rendering have not been
migrated. The complete inventory and per-plugin acceptance lanes are tracked in
[[plugin-pure-renderer-inventory]]. No later milestone is claimed complete.

## M1 Prototype Implementation Review

`crates/brood-wasm/src/snapshot_store.rs` compares immutable copied payloads with
shared immutable chunks. Admission counts allocations retained by superseded
roots and exported chunk handles. Root, allocation, chunk, and payload ceilings
are independent. Publication rejects overload or foreign/stale revisions without
replacing the latest accepted root. Invalidation revokes old leases; dropping a
store also revokes authority.

Root construction runs outside the acquisition lock. The exchange uses
preallocated retirement storage. A separate reclamation call retains pinned
roots and destroys unpinned roots outside the acquisition lock. Payload capacity
is released only after deallocation. Chunk descriptor arrays use exact-length
boxed slices so excess caller Vec capacity cannot escape the metadata bound.
The owner must keep the store alive and drive its reclaimer while pins drain;
the store destructor is not a production asynchronous shutdown protocol.

`crates/brood-wasm/examples/snapshot_store_probe.rs` changes one row per revision
in the same logical state for both strategies. It verifies byte equality and
keeps three historical pins. It separately measures construction, assembly,
exchange, acquisition, contiguous transfer-buffer construction, and reclamation,
including final zero-retention checks. It reports percentiles for session-list,
terminal-row, and approximately 1 MiB shapes. This is a storage experiment, not
a terminal rendering or presentation benchmark.

`crates/brood-wasm/src/facet_probe.rs` runs an import-free core Wasm probe with
fuel, epoch, memory, and output bounds. Its instance is separate from application
actors. Discard/repeat checks compare output metadata and bytes; an error drops
the execution context. The runnable `facet_probe` example uses real
`ComponentActor` instances: a worker enters its cooperative spin task, the actor
enters a tight dispatch loop, an isolated facet executes, and a peer actor makes
progress. A live worker task is not evidence of simultaneous execution inside
the shared actor Store.

The probe initially waited for the actor's `CompletionDelivered` event as its
entry oracle. That event is produced by the actor runtime loop and can be held
up by the very call being observed. Direct observation of the guest-written
checkpoint showed that the actor had entered. The corrected probe passes;
the earlier timeout is not evidence of a target contradiction or scheduler
defect. Earlier measurements that inferred actor entry from mailbox admission
are not acceptance evidence.

Review rejected an initial storage benchmark that changed all copied rows but
only one shared row, and that omitted shared-view preparation time. Review also
corrected guest-limit fixtures whose undersized memories could reject at the
wrong stage. The retained tests verify the intended failure stage and successful
subsequent clean invocation.

### Transfer Ownership And Review Rejections

`ByteBudget` and `BudgetedBytes` reserve shared staging/input/result capacity
before copying and keep that reservation until the actual allocation is freed.
The byte wrapper exposes borrowed reads, not an uncharged extracted Vec. Dropping
the last budget owner revokes surviving buffers. `publish_copy_of` admits the
destination root before copying borrowed staging bytes, avoiding the unbudgeted
intermediate clone found during review. The original prepared-buffer APIs remain
host-side comparison helpers, not safe guest publication entry points.

The Component Model probe initially lifted a `WasmList` and borrowed its guest
memory after `TypedFunc::call`. Review rejected this: Wasmtime 46 automatically
runs guest post-return cleanup before returning to the host. A result can already
be freed or overwritten. `component_facet_lifetime` now proves the failure mode
with a guest that overwrites its result in post-return; owned Vec lifting keeps
the original bytes. A forged list range is rejected before host allocation.

The corrected runtime uses owned `Vec<u8>` lifting and reserves the full 4 MiB
single-guest-memory ceiling before invocation. Wasmtime validates a flat byte
list's range before allocating its owned lift. The smaller 64 KiB semantic result
limit is checked **after** that bounded copy; it is not a pre-copy allocation
limit. This reasoning is specific to flat bytes and the pinned Wasmtime runtime.
Nested-list amplification, multiple memories, arbitrary producer callbacks, and
unbounded decoded structures are not covered by it.

The Component probe now runs on one dedicated worker thread, measures 100 calls
per shape, and rechecks authority after each call. Its fixed trusted fixture
corpus is reported separately from admitted pipeline bytes and is moved into the
core-Wasm comparison instead of cloned. Full generation/candidate rechecking at
acceptance and commit remains M2 work; a borrowed slice is not revocable memory.

### Measurements And Scope

Reference machine: AMD Ryzen AI MAX+ 395 with Radeon 8060S, x86_64, 16 cores / 32
threads; approximately 27 GiB exposed RAM. These are release-mode, nonpresentation
experiments using the checked-in Nix environment and Wasmtime 46.0.1, based on
commit `d1c687802f291322859b8aa074233759036c2745` plus this working tree. A physical
refresh rate and geometry-lag budget are not applicable to these checksum/storage
experiments; no 180 Hz presentation acceptance is claimed.

The 500-revision storage comparison changes one row per revision and retains
three older views. In the reviewed run, the 1,024,000-byte shape allocated
512,000,000 copied payload bytes versus 1,534,976 shared payload bytes over the
run; peak live payload was 4,096,000 versus 1,027,072 bytes. Root-exchange p99 was
40 ns copied / 90 ns shared. Construction p99 was 106,349 ns copied / 7,854 ns
shared; contiguous transfer p99 was 108,793 ns copied / 133,660 ns shared.
These are one-run observations, not performance guarantees. Sharing reduced
retained/allocation payload strongly but did not eliminate full cross-instance
transfer. Metadata has independent count ceilings and is not included in
payload-byte totals. This run includes independent allocation reclamation.

The dedicated-worker canonical run takes 100 samples per shape, republishes a
new revision for each call, and verifies every result. A 7,231-byte session view
measured 2.564 / 2.655 / 3.076 / 32.291 microseconds (p50/p95/p99/max). The
65,026-byte terminal fixture measured 18.535 / 18.775 / 20.729 / 25.568
microseconds; 1 MiB measured 271.070 / 298.401 / 516.171 / 527.171 microseconds.
The final run
was slower at the 1 MiB tail than the earlier run (p99 327.944 microseconds), so
these observations must not become a fixed latency guarantee. The separately
reported trusted fixture capacity was 1,148,928 bytes. These
measurements include canonical input lowering, guest checksum execution, owned
result lifting, and cleanup, not UI rendering. Core-Wasm and Rust Component
fixtures have different guest code generation, so their call-time difference
cannot be attributed solely to the ABI.

### Outstanding Gates

`BackgroundSnapshotReclaimer` now supplies bounded ownership across generation
teardown. Registration reserves one of a capped number of generation slots before
constructing metadata, uses a nonblocking bounded queue, and shares one worker.
Managed handles do not expose direct reclamation. The worker retains roots and
allocations independently until pins drain, then frees them before releasing the
generation slot. It waits without polling when empty. Tests cover exported chunk
retention, teardown with old leases, service-drop lifetime, invalid limits, and
concurrent registration overload. The unmanaged comparison store still requires
explicit caller discipline; no product runtime uses the managed wrapper yet.

The initial review rejected the same-artifact dynamic trap linker because
Component Model arguments are lifted into owned host values before the denial
closure runs. A modest regression confirms that guest-memory limits alone do
not bound that host allocation. The user subsequently clarified that plugins
are trusted and explicitly accepted this risk. The target has been intentionally
updated to record that fault-containment model; the allocation concern no longer
blocks rollout. The runtime observation remains documented in
[[plugin-pure-renderer-linker-review]].

Implementation resumes separate actor/facet instances of the same compiled
component. Wasmtime's built-in trap helper still has a functional sync/async
typing mismatch for Brood's imports, so linkage must select correctly typed
stubs. Ordinary forbidden calls must trap, peer instances must remain usable,
and a failed facet context must be discarded. No pre-lifting adapter or separate
artifact convention is required to address the accepted risk. Normal guest,
snapshot, output, and resource limits remain in scope.

The final product executor, paired input admission, graphics-resource leases,
and exact composition remain later gates. No synthetic checksum output is
treated as proof of text rendering, semantic input, GPU recovery, or nested
resize behavior. The current suite includes snapshot coherence/overload,
staging/transfer budgets, guest lifetime/malformed output, denied imports,
fuel/epoch bounds, and existing layout/pointer tests. Product plugin migration
has not begun, and no later milestone is claimed complete.

### Coverage And Verification Repairs

The bundled coverage audit originally rejected scripts that assemble shell
children through a componentization helper or the package manifest. It now
recognizes those mechanisms while still rejecting a missing helper-built child
or missing manifest placement. The render-scheduling workload also explicitly
checks the required shell child artifacts in supplied packages. Inventory
coverage is 17 plugins across 10 existing behavior lanes; this is coverage
accounting, not a claim that those lanes have passed facet migration.

The repository shell check exposed two existing unquoted pattern-prefix
expansions in the divider screenshot harness. Quoting the output-directory
prefix prevents glob characters in a supplied directory from corrupting the
recorded relative screenshot path. Those verification repairs did not change
the target; the later trusted-plugin clarification intentionally did.

### Verification At This Review Boundary

The Nix-backed `brood-wasm` integration suite now includes ordinary sync/async
denial and constructor isolation checks. The argument-lifting diagnostic is
no longer a required regression under the accepted trust model. Strict
all-target Clippy, formatting, shell lint (33 files), build-helper self-tests,
and the bundled coverage audit pass. The audit accounts for 17 plugins across
10 lanes; no product behavior-lane acceptance is claimed.
Release storage and canonical transfer runs pass their result-coherence and
zero-retention assertions; raw observations are preserved in
`plugin-pure-renderer-m1-measurements.txt`.

This boundary completes substantial M1 proof work. The initial rejection of
dynamic trap linkage has been superseded by the trusted-plugin decision. It does **not** complete the user's requested all-plugin
rollout. M2 product WIT, facet export discovery, bounded scheduling,
resource leases, paired frame/input commit, and semantic-event admission remain
unimplemented, followed by the switcher, terminal, remaining visual producers,
and exact composition/recovery gates. All 17 bundled plugins remain tracked in
the inventory; none is silently counted as migrated.

### Same-Artifact Proof After Trust Clarification

`examples/wasm-component-actor` now includes a prototype render export alongside
its existing actor and worker exports. The product WIT remains unchanged; the
example's local world includes it and adds the probe export. `facet_linker`
recursively installs correctly typed synchronous and concurrent trap stubs for
its imported functions. Constructors and ordinary denied calls fail in their
own execution context.

The release `component_facet_probe` passed with the same component file supplied
for both roles. It ran 100 calls per shape while the associated actor was inside
its checkpointed busy dispatch and its worker task remained live; a peer actor
also progressed. The isolated render instance never reads actor statics. Owned
result lifting, snapshot/result budgets, and oversized-output rejection remain
in place. The 1 MiB shape measured 252.122 / 256.510 / 372.889 / 711.713
microseconds (p50/p95/p99/max). This is execution/transfer evidence, not rendered
pixels or M2 input/resource acceptance. The next gate remains the paired
render/input protocol using separate instances of the same artifact.

Verification after this change: all 54 `brood-wasm` integration tests, strict
all-target Clippy, formatting, and diff checks pass through Nix. The actor
example rebuilt successfully and the same-artifact release probe passed.

### M2 Implementation In Progress

The optional product interfaces are now `render-publication` and `facets` in
`brood-visual-plugin`; the original actor world remains the SDK default.
The SDK `facets` feature opts into both exports and shares the bounded versioned
wire codec from `brood-protocol`. Host inputs carry explicit invocation limits.
The wire decoder admits list metadata and payload allocations before allocating;
the host wrapper retains a generation-scoped aggregate reservation with the
decoded value. Guest output and decoded values have separate live charges.

`FacetRuntime` discovers both optional functions, runs on a bounded dedicated
`FacetExecutor`, and destroys a context after a trap or invalid output. Queue
admission does not wait for a guest. Dropped queued receivers skip guest work;
in-progress shutdown remains bounded by fuel and epochs, and context capacity
is released only when the worker actually exits.

Actor configuration can register publication against its actual allocated
generation. The host import uses a borrowed Wasmtime byte list and admits the
copy before allocation. Publication authority is restricted to the active
actor-dispatch task and revoked on shutdown, runtime exit, or actor drop.
A retained host publication handle remains bounded and revoked until released.

`FacetFrames` admits complete pane sets and retains draw bytes, interaction
bytes, geometry, and the original application snapshot together. Speculative
results never route input. No-change preserves the original source revision and
requires unchanged geometry, configuration, renderer, membership, and transient
interaction dependencies. Review found and fixed a missing geometry-revision
comparison. Graphics preparation and client composition are not integrated yet.

`FacetInput` queues pinned committed frames and admits sequence-state changes
with bounded semantic delivery batches exactly once. Review rejected the first
input implementation's duplicate payload accounting and returned requests whose
actual allocation lifetime escaped the reservation. The coordinator now borrows
admission input and returns an owned budgeted request. Cancellation preserves
already committed delivery batches; it cannot silently erase actor actions.
Existing pointer modifiers, logical keys, paste, and image inputs were restored
to the draft protocol before migration.

Verification so far: 75 `brood-wasm` integration tests passed through Nix,
including real Component fixtures for runtime/executor/publication and host
frame/input admission tests. All 22 protocol tests and protocol all-target
Clippy passed. Separate real actor-authority and paired facet probes are being
completed. This is not M2 acceptance: invocation-scoped graphics resources,
client composition/input integration, and the real plugin migration gates
remain outstanding. No bundled plugin is counted as migrated.

M2 continuation: the paired release probe now uses the host graphics gate and
actually invokes image creation, text layout, and resource drop from the guest.
Constructor and input interpretation attempts have no lease and return
unavailable. Invocation resources have exact token/pane/kind ownership and a
bounded background owner through final native retirement. Review replaced the
initial resource implementation's blocking Drop fallback, unadmitted metadata,
and leaked failure cleanup with registration-time admission and retained
reclaimer ownership. Packet command/path allocations have a borrowed preflight
bound; decoded packets keep a budget reservation.

Typed `facet-input` actor events carry the complete source token and ordered
semantic batch. Actor queue slots and destination bytes are reserved before
copying the batch. The real paired guest receives removed stable ID 202 with
source application revision 1 and explicitly rejects that intent rather than
activating another row. The proof uses coordinator delivery inspection, actor
admission, then acknowledgement. Its reported result is `PASS paired facet
resources, stable actor input, no-change, trap recovery, and atomic delivery`.
Publication also wakes host interest immediately, coalesced until consumed;
it need not wait for actor dispatch to return.

The native text/image adapter checks invocation retained-payload admission and
reuses the existing native resource maps. Shared glyph images now have a
4096-entry / 64 MiB cache bound; eviction drops only cache references, preserving
live scene references. Native font/shaping internals do not expose exact heap
telemetry, so resource accounting is conservative logical backing plus finite
request/scratch limits, not an exact allocator-byte measurement.

The combined WASM test log at `/tmp/brood-facets-integration.log` records 87 passing
integration tests. Native offscreen verification exposed stale harness waits:
direct leased-layout logging had become synchronized-layout logging, and the
empty-startup branch captured after a picker-create message without waiting for
its concrete frame. Exact geometry checks and causal waits are being repaired;
the deferred synchronized commit route is still under investigation. The
keybindings screenshot from the populated run was visually inspected: native
text and the underlying image are intact. No full nested pass or production
facet rollout is claimed from those partial results.

M2/M3 native integration continuation (2026-09-05): the client now loads two
bounded isolated contexts before a visual instance's lifecycle, provides
publication storage only to components importing it, admits input against the
committed pair on the event loop, and transports accepted semantic batches into
the actor mailbox. The same component may also serve a nonvisual init instance;
that instance does not instantiate unused visual contexts. Scheduler registration
must follow isolated compilation, so a still-loading actor cannot receive demand.

Native packets now share an allocation owner across snapshots and preparation
instead of deep-copying decoded graphs. Complete facet batches retain their
paired commit receipt and resource leases. Ordinary promotion and the prepared
synchronized promotion path commit interaction with the complete native batch;
abort, supersession, and cleanup cannot release uncommitted facet CPU scenes
into replay. Native packet/resource tests pass (28), and the full client unit
suite passes (292 tests). These counts precede the pending receipt-boundary tests.

The optional session-switcher feature now publishes only the final view in each
actor dispatch, shares its view-based drawing routine with the legacy path,
interprets stable targets, and returns explicit hover/press sequence state.
Its feature tests (41) and default tests (39), plus both WASM release Clippy
checks, pass. A test-only feature switch is available in the nested harness;
production defaults remain unchanged.

The repaired legacy empty-startup workload passed at
`/tmp/brood-native-resource-empty-gate2` (keybindings screenshot RMSE 0.105460).
The populated legacy run at `/tmp/brood-native-resource-populated-gate3` produced
correct native text/image and pointer screenshots but its full gate remained
blocked at backend discovery: the click and picker creation were logged, while
the fixture's session lacked the daemon init actor realm needed for the reply.
This is a preserved failure, not a relaxed assertion.

The first actual facet-native frames are recorded at
`/tmp/brood-session-switcher.ejJobl`: switcher candidates 11 and 17 committed
ordinary native pairs, and inputs 1–4 retained their source frame identity.
The root inspected `empty-startup.png`; text and sidebar/status geometry are
correct. That run did not pass the full workload: candidate 18 was invalidated
by layout geometry, but its scheduler opportunity remained outstanding. The
runtime now explicitly retires discarded facet opportunities before requesting
replacement work. The full offscreen facet gate remains open pending rerun.
No production plugin is yet counted as migrated.

The feature-enabled switcher empty-startup gate subsequently passed in full at
`/tmp/brood-session-switcher.1F3xFR/proof.md`: nonblank sidebar, backend picker,
directory picker, and keybindings captures; ordinary and synchronized facet
commits; inputs 1–11 delivered; all synchronized cohorts missing=[] and
non-degraded; RMSE 0.105460. Retired resize tags are now reconciled with the host
transaction set even when dimensions do not change. Three additional CPU receipt
boundary tests pass. Native receipt discard now wakes coalesced replacement
demand without retiring a newer in-flight candidate; this later recovery change
has separate checkpoint unit verification, not another claimed full nested pass.

Implementation paused on the picker row-identity contract gap described in
`docs/frictions/picker-stable-identity.md`. The optional status-bar work was
saved as an unverified prototype patch and removed from the active source tree.
No production visual plugin default has been enabled. Nonvisual plugin builds,
componentization and available package tests passed as recorded in the inventory.

Paused checkpoint verification: full client suite 295/295 passed; the expanded
receipt suite subsequently passed 4/4, including last-uncommitted-owner retry
notification and no retry after commit. Client all-target Clippy with
`-D warnings` passes (`/tmp/brood-client-facets-checkpoint-clippy3.log`). Active
status-bar/inbox/picker/keybindings source trees are unchanged; the status-bar
prototype remains saved separately. The picker policy decision is still open.

## Stable picker identity decision and resumed migration — 2026-09-05

The user explicitly selected caller-supplied stable IDs. The target records the
contract; the earlier friction is resolved. Picker uses `{id, value}` envelopes
and returns `item_id` with the original inner result. Session-switcher and
keybindings callers have migrated. Switcher validates domain identities before
acting, including rejecting removed backend/session/agent selections. A test
covers duplicate labels, reordered backends, label changes, and removal.

Switcher feature tests: 42 passed. Nix WASM release all-target Clippy with
`-D warnings`: passed (`/tmp/brood-switcher-stable-clippy.log`). Picker, keybindings,
status-bar and inbox facet implementations remain under review. Review caught
current-state keyboard selection in picker and missing keyboard/scroll behavior
in inbox; these require correction before the next native gate. Compilation
alone is not evidence of a completed migration.

## Native migration checkpoint and resize-proof pause — 2026-09-05

All five shell facets passed the offscreen empty-startup gate at
`/tmp/brood-session-switcher.boozZH/proof.md`. Review corrected premature
UiFrame resource retirement, publication before pane creation, and first-scene
geometry selection. The full populated shell gate remains outstanding.

Neovim and diff-viewer opt-in migrations passed focused default/feature tests
and feature WASM Clippy. Coding-agent native testing exposed input queue
pressure and a later render trap; exact-size bounded response encoding and a
larger finite queue were added, but native acceptance remains incomplete. The
latest rerun stopped at a Wasmtime/anyhow error conversion compile error, now
corrected but not yet reverified. Terminal capacity and compact paste fixes are
still being finalized. No visual plugin default has been enabled.

Paused further implementation after identifying the mismatch documented in
`docs/frictions/pure-renderer-resize-proof.md`. Retaining actor delay gates does
not preserve the legacy delayed-render proof under independent pure rendering.
The target has not been changed to conceal that mismatch.

## Continued verification after approval — 2026-09-05

The user approved proceeding to completion with separate actor-stall independence
and delayed-facet completion proofs. Production defaults remain disabled while
native gates are unfinished.

Coding-agent passed the real offscreen style and clipboard-image workload at
`/tmp/brood-coding-agent.r0iaMO/proof.md`: typing/editing, streamed Markdown,
scrolling, search, stable tab dragging, and a rendered clipboard PNG. No facet
failure was logged. The Markdown screenshot was inspected. This is functional
evidence, not the release/180 Hz performance acceptance gate.

Further native testing identified and corrected premature fatal failure on
temporary resource-lease admission pressure, and speculative frame replacement
while a prior receipt still awaited native commit. Native discard notification
now identifies its invocation so a late discard cannot cancel a successor.
The frame/resource tests and 297 client tests passed after that correction.

The bounded wire codec now sizes owned messages before allocating and copies
byte buffers in bulk. The previous per-byte encoding loop exhausted 25 million
fuel units on a valid 256 KiB terminal paste. Wire compatibility and allocation
admission checks remain intact (six codec and two wire ownership tests passed).
The terminal native retest progressed beyond that trap and exposed a separate
transport-record size limit; bounded transport chunking is still under review.

Publication failures retain the prior accepted root instead of panicking.
Picker host budgets now match its 8 MiB publication and stable-ID interaction
contract. Generic semantic-event admission accommodates worst-case JSON escaping
of the already admitted 256 KiB text input. Input that changes neither application
publication nor transient interaction state no longer requests an extra frame.

Populated shell testing remains incomplete: keybinding remapping now waits for
the filtered committed view, but a later backend-picker replacement can stall
before native commit. Debugging is active; neither longer waits nor accepting a
layout request alone is treated as proof of presentation. Editor testing reached
real file operations and editor actions, but its complete combined workload and
the final refreshed plugin package still require verification.
# Approved resize-proof split

On 2026-09-05 the user approved separating the migrated resize proof into two
native assertions. A stalled actor must not prevent its facet from rendering the
last published immutable view at exact host geometry. Independently, a bounded
test-only delay in facet completion delivery must hold terminal cohort promotion
until both native facet pairs commit. Preview motion remains a projected
interaction path; the terminal release is the durable synchronized cohort.

The native split proof passed at
`/tmp/brood-pure-facet-resize-proof-26`: 37 deterministic captures retained
exact two-pane host geometry, the terminal cohort promoted only after both
tagged native facet commits, and the stall fixture rendered independently from
its actor's held publication.

The first release 180 Hz default-client run used pure-renderer builds for all
nine package artifacts and is retained at
`/tmp/brood-pure-180hz-new-session`. It failed the unchanged 0.25 physical-pixel
new-session-button oracle: maximum position errors were 0.3168, 0.3450, and
0.3938 pixels across three 1.5-second, 270-step drags. The corresponding maximum
size errors were 0.2475, 0.2695, and 0.3077 pixels. These samples were real stale
surface projections: source and target widths differed by one or two pixels
during 179/416, 180/413, and 187/410 presented frames. The runs had zero sampled
pointer/divider lag, maximum repeated sampled-frame run one, and final exact
three-pane facet cohorts. The remaining defect is the host preview path clearing
its wait/affected sets before exact facet output is ready.

### Native promotion and local-control recovery — 2026-09-05

The populated shell proof exposed a reproducible presentation cycle: degraded
resize promotion discarded all prepared facet pairs when a managed sibling
needed recovery. Reopening the picker then exposed its old blank scene; the
geometry guard skipped presentation while fresh ordinary facet frames waited
for that same presentation to release the epoch. Trace evidence is in
`/tmp/brood-session-switcher.zCXq4T/client.log`. Native promotion now preserves
complete validated facet batches during degraded recovery; abort and cleanup
still discard speculative pairs. `/tmp/brood-session-switcher.SNiDFQ/client.log`
verified synchronized picker replacement and zero geometry-deferred records.
The 297 existing client tests passed; an additional regression distinguishes
prepared degraded promotion from abort. Final regression rerun remains pending.

The later CLI session-switch assertion exposed a separate local-control defect:
backend state reported Gamma while the presentation catalogue stayed at Beta.
Unlike DBus and backend IPC, local control did not publish the catalogue after
mutations. The local request callback now publishes and flushes clients. Native
verification of this follow-up remains pending. The test deadline remains five
seconds; it was temporarily extended during diagnosis and did not resolve it.

Terminal native verification passed with actual per-cell ANSI styling, visible
Unicode, and an exact 262144-byte quote/backslash paste verified at the PTY:
`/tmp/brood-terminal-pure-native23`. Its screenshot was visually reviewed.
The split resize proof passed at `/tmp/brood-pure-facet-resize-proof-26/proof.json`:
actor stalls leave facets independent and a scoped 90 ms completion delay proves
the paired cohort barrier. The separate 180 Hz performance gate is outstanding.

Follow-up native tracing separated two causes in that cycle. Preserving the
prepared pair was necessary, but `painted_extent` was also inferred solely from
an opaque origin rectangle. The picker intentionally uses a rounded transparent
canvas, so its valid committed pair could still be classified as missing output.
`GraphicsSnapshot` now carries the paired facet's validated physical extent;
GPU preparation uses that authoritative extent and only applies the opaque clear
optimization when the fill actually covers that extent. A regression covers a
rounded canvas with a smaller opaque header. Shell run 27 passed picker close
and the local-control Gamma transition (catalogue revision 3); it later stopped
on a separate managed-client restore-buffer assertion.

The first release 180 Hz all-facet workload failed the hard 0.25-pixel button
oracle: maximum position errors were 0.317, 0.345, and 0.394 pixels. The target was
not changed. Actual stale surfaces differed from the preview target by two
pixels; the old preview path cleared its wait set and stretched them. Preview
admission now enrolls changed facet panes and retains the prior composition if
its bounded preview cohort times out. Re-verification is in progress.

Recovery testing also found that native pair commit ignored `next_frame_ns`.
Committed deadlines now feed event-loop scheduling, with one-shot due admission,
replacement/cancellation on later commit, and generation/visibility cleanup.
This is being checked with native animation start/stop and recovery workloads.


### Ordinary geometry and managed-client pacing — 2026-09-05

The exact-frame barrier must also cover ordinary layout publications, including
startup, pane visibility changes, and completed child fragments. The first
180 Hz fixes only covered explicitly synchronized requests; later traces still
showed ordinary publications projecting old facet textures. Host-owned ordinary
cohorts now reuse the synchronized lifecycle without actor interaction sequences
or actor terminal notifications. Root-scoped eligibility and removal of panes
absent from the target address cross-lease geometry and hidden-pane admission.
Native performance and recovery re-verification remain in progress.

Facet GPU preparation retains the complete owned-pane batch when only a subset
is visible. Readiness now requires any visible member for a facet batch, while
preparing and committing every member atomically; fully hidden batches remain
undrawn. The client suite passed 305 tests before the latest ordinary-cohort
follow-ups. Committed `next_frame_ns` deadlines drove hundreds of native animated
pairs and survived renderer replacement; the full recovery gate has not yet
passed.

The populated shell trace `/tmp/brood-session-switcher.PcqOpn/server.log`
confirmed a managed-client pacing cycle: Weston acknowledged fresh configures
but stopped committing while export waited for a tagged cohort or ordinary
recovery commit, and no later frame callback arrived. An accepted resize now
grants the addressed window a frame callback without acknowledging presentation
or exporting an incomplete scene. Smithay drains committed callbacks, so repeat
configure requests do not invent additional pending callbacks. Backend check and
28 library tests pass; native shell verification is pending.


The first ordinary-cohort native run exposed another bypass: the legacy dirty
layout publication path could request degraded promotion immediately, with zero
of two wait panes ready. That run is not acceptance evidence. Exact host-owned
and preview cohorts now reject incomplete/degraded preparation centrally; dirty
publication and late-region paths no longer bypass the barrier. GPU recovery
also returns invalidated exact cohorts to Waiting. Changed managed windows join
the same wait set as changed pure panes for both ordinary layouts and previews.

The shell harness can select a release host for native acceptance; debug runs
were taking roughly 1.6 seconds per managed frame in this workload. Its startup
oracle now recognizes completed synchronized layout publication alongside the
legacy leased-layout trace, while retaining fresh managed buffer and real input
assertions. No geometry tolerance or deadline was loosened.

The final serialized 180 Hz divider/new-session workload passed at 94.18/100
with the release host and all bundled presentation facets enabled. Across three
drags, all 1,264 instrumented new-session-button presentations had zero position
and size error, and content projection reported a zero-pixel maximum source to
target extent delta. Sparse geometry captures never repeated a divider position
(maximum run one). Committed session-switcher frame sequences repeated for at
most six presentations, spanning 25.6, 33.4, and 26.5 ms; the workload therefore
also rules out a geometry-only pass backed by an indefinitely frozen facet.
Desired-versus-committed divider lag peaked at 1.0, 2.0, and 1.0 pixels, recorded
separately from the exact element geometry oracle. Evidence is in
`/tmp/brood-pure-180hz-exact-cohorts/metrics.json` and `report.md`.

The run used an AMD Ryzen AI Max+ 395 (16 cores, 32 threads). Host load averages
immediately after the run were 4.97, 6.65, and 7.69 while unrelated desktop
workloads remained active. Brood builds and native performance gates were kept
serial.


The three-scenario recovery workload passed its animation, replacement, replay,
hide/show, and screenshot checks, but failed the final report because pure facet
keys had no measured input-to-presentation correlation. The missing plumbing is
being added to generic scheduler watermarks. A proposed "newer snapshot implies
input reduction" shortcut was rejected during review: unrelated publication or
an already stale interaction map could produce a falsely short latency. Host
publication metadata instead identifies the exact facet input reduced by the
actor; transient sequence changes have their own direct causal path. Legacy
FIFO timing must remain intact. Final report acceptance is still pending.

Release shell run 33 (`/tmp/brood-session-switcher.oLCOiZ`) passed sidebar hide
and restoration with exact fresh managed buffers, validating the resize callback
fix. Adding a new managed window then exposed an admission gap: the ordinary
cohort trigger only considered changed facets, so managed-only topology changes
still bypassed the barrier. That follow-up is being fixed. The release editor
run opened its real file but failed save because an unchanged stale child layout
response took focus back from the newly focused editor. The harness save check
was retained; the host focus ordering is being corrected instead of adding a
second focus shortcut to conceal the regression.

The shell harness now accepts an existing assembled package, and bundled suite
shell/startup/narrow lanes use the suite package. This allows native verification
of the actual frozen package while source work continues, avoiding partial
builds and feature-artifact races. The inventory audit still covers all 17
plugins in 10 native lanes.


### Combined verification checkpoint — 2026-09-05

The per-plugin matrix passed 51 unique commands: native legacy and pure tests
plus both WASM Clippy variants for all ten visual plugins, native tests for all
seven nonvisual plugins, and both native/WASM variants of the graphics example.
Logs are in `/tmp/brood-final-plugin-checks`. A resize-proof diagnostic lint was
scoped with a reason; an accidental pure-only request-ID change in a switcher
test fixture was restored to the actual pending request. The final switcher log
contains 42/42 passing tests and preserves the failed attempt separately.

Root review of scheduler input correlation found that readiness is not a sorted
prefix: a later transient input can be ready while an earlier application input
still awaits publication. Exact per-input extraction now replaces a binary
partition on that nonmonotonic condition. The regression exercises actual
scheduler dispatch and speculative abandonment. Ready markers remain bounded
through eviction/requeue; ordinary actor turns cannot acknowledge pending facet
keys. Legacy FIFO correlations retain their previous behavior.

The frozen combined host passed all 312 client tests, 534 workspace tests, and
whole-workspace all-target Clippy through Nix. One existing Linux user-systemd
isolation test remains explicitly ignored. Logs:
`/tmp/brood-client-final-tests.log`, `/tmp/brood-core-final-tests.log`, and
`/tmp/brood-core-final-clippy.log`. Release client SHA-256:
`a3cd87694db67ef66a2db380bbc712b18b8b51f67c43a94f842888c61f11af83`.
Native recovery/editor/shell reruns remain pending; defaults remain off.

### Final recovery gate blocked (2026-09-05)

Release client `7ca959fd8081431682cf639449cbd5e6628c4e7e0d733261795862ab1a53bbaa`
completed recovery, visibility, overload, and resource-churn workloads with zero
actor failures, renderer failures, reliable transition drops, and partial
actor-frame promotions. It collected 261 unsaturated and 256 saturated causal
key-to-present samples.

Acceptance failed: unsaturated median latency was 12.247 ms against the unchanged
12 ms limit, and the unsaturated run recorded 29 token protocol errors. The
protocol-error cause is not yet established; this does not establish a design
target defect. Report:
`/tmp/brood-render-pure-gate-final2/render-scheduling-report.json`.

Default rollout is held. The editor rerun was safely stopped with partial
artifacts in `/tmp/brood-editor-pure-native-final3`; it is not a passing gate.
Read-only protocol-error investigation precedes further implementation.

### Recovery diagnosis and readiness correction (2026-09-06)

The diagnostic run `/tmp/brood-render-diagnosis-1` localized all 29 protocol
errors to `mark_graphics_ready`: tokens 191 through 219 accepted a single visible
pane, while native promotion correctly retained both owned panes in the atomic
batch. The bridge incorrectly notified scheduler readiness for the hidden member.
It now filters notifications using the token's current expected
pane set. Native commit remains a complete batch; direct unexpected readiness
notifications remain invalid. The regression also changes visibility after token
acceptance to ensure newly exposed panes do not acquire obligations on old tokens.
Hiding a pane can still release an existing readiness obligation.

The partial run is not acceptance evidence. Its input timing breakdown showed a
median 8.509 ms before dispatch, 1.170 ms dispatch to facet submission, 0.192 ms
validation, 0.648 ms to graphics readiness, 2.105 ms readiness to composition,
0.142 ms GPU submission, and 0.045 ms present-call return. Medians are separate
stage distributions and are not additive. Presentation timestamps are explicitly
estimated from the present call, not compositor scanout feedback. Further latency
profiling and the full native rerun remain pending; thresholds are unchanged.

The editor native gate passed against the readiness-corrected release:
`/tmp/brood-editor-pure-native-final5/proof.json`. Three addressed opens and three
saved files verified real typing/correction, exact requested cursor placement,
and a 119001-byte streamed file. Terminal, diff, and agent routes completed.
The populated diff screenshot was visually reviewed and showed the saved inbound
and large-file edits. Legacy-only draw oracles for terminal/diff were updated to
recognize actual paired facet commits; the diff still requires worker completion
and visual content verification.

Text backing reuse now shares only live ready immutable host resources, requiring
exact text/options, generation, scale, font-context revision, and accounting-mode
matches. Every invocation still receives a fresh handle and full byte charge.
Lookup examines at most the newest 64 live handles; old matches fall back to
normal shaping. All 318 client tests and all-target Clippy passed through Nix.
The release performance rerun remains pending.

The populated shell gate reaches sidebar/picker/keybindings and managed resize
recovery but blocks when Ctrl+O adds a third managed window. Exact managed cohort
admission succeeds; the new pane prevents the backend's tagged snapshot group
from exporting. Artifacts `/tmp/brood-shell-pure-final35` preserve Wayland traces.
No partial group export or relaxed acceptance was introduced. Further diagnosis
must distinguish absent commits from unsuitable or superseded configure serials.

The backend now marks managed XDG toplevel configures with all tiled edges,
including tagged configures. This expresses the existing tiled layout and makes
Foot honor exact pixel bounds instead of floating-window cell increments.
`/tmp/brood-shell-pure-final38` imported the requested 480x686 Foot buffer and
completed the two-member managed cohort without stretching. The next action
exposed a separate new-pane focus handoff bug. Host-owned atomic promotion now
considers a newly admitted active managed pane, while preserving explicit newer
host focus and actor-owned focus authority. Three focused regressions passed.
The next full shell run stopped earlier on an intermittent Ctrl+Shift+Slash
policy-routing failure; that run does not verify the focus fix.

`/tmp/brood-render-pure-gate-tiled/render-scheduling-report.json` completed all
three native recovery lanes. Both 60 Hz lanes reported zero protocol errors,
actor failures, and renderer failures. Strict acceptance still failed: the
unsaturated median was 12.583 ms against 12 ms. This run included profiling and
protocol diagnostics. A standard uninstrumented acceptance run is pending;
thresholds and estimator behavior remain unchanged. Diagnostic output is now
removed from focused deterministic test subprocesses because interleaved trace
lines prevented the report assembler from recognizing passing test output.

The runtime-I/O coverage attempt found an ABI-stale component in the old Nix
package overlay, not an established source regression. Runtime-I/O and workspace
watch components were rebuilt from current source and copied into the writable
render test package. Their native lanes remain pending.

The clean release run `/tmp/brood-render-pure-gate-clean` also failed acceptance
(zero protocol/actor/renderer errors): unsaturated key p50 12.235 ms, p95
37.266 ms, with only 569 measured opportunities and frame p95 33.866 ms.
Reviewing every gate exposed a preexisting half-rate animation bug hidden behind
the analyzer's first p50 failure. The host waits until a committed facet's
requested next presentation time to create demand, so preparation misses that
cell and targets the following opportunity. Each saturated actor also runs at
half rate, but staggered actors hide this in the global frame distribution.
The fix must schedule preparation ahead of the requested presentation time;
changing thresholds or the guest's animation period would hide the host bug.

The shell shortcut failure was focus-dependent policy behavior: raw-control
plugin handling returned before resolving declared Brood global shortcuts.
The policy now resolves those explicit shortcuts while retaining ordinary
application control passthrough. Six policy tests and WASM release Clippy passed.
`/tmp/brood-shell-pure-final41` passes the shortcut but exposes a further managed
resize/restoration issue under investigation. It is not an accepted shell gate.

Both nonvisual lanes passed with current components:
`/tmp/brood-pure-nonvisual-final/runtime-io/proof.md` and
`/tmp/brood-pure-nonvisual-final/workspace-watch/proof.md`.
Their hot-replacement oracle now waits for asynchronous candidate commitment
before asserting the unchanged exact watcher registration/subscription counts.

Additional native gates passed against the current pure overlay:
`/tmp/brood-pure-shell-extra/startup-empty/proof.md`,
`/tmp/brood-pure-shell-extra/keybindings-narrow/proof.md`, and
`/tmp/brood-pure-worktree-final/proof.md`. Empty-startup and narrow shortcut
screenshots were visually reviewed.

The shell41 restoration failure was an early forced-degradation path:
`layout_state_dirty` caused an applicable actor-owned resize to finish degraded
at 2.687 ms when an unrelated hidden-session actor joined. The exact resized
managed buffer subsequently arrived but was no longer eligible for promotion.
Compatible dirty publication now waits for transaction readiness or its existing
bounded deadline; incompatible target inventory still takes the explicit abort
path. The focused regression passes. Full shell verification remains pending.

Desired animation demand now enters the scheduler immediately after native
commit. The predictor maps the requested time onto its existing opportunity
cells and subtracts learned preparation costs before dispatch. It retains one
replaceable target per actor; input/publication demand supersedes a future
animation target. Hiding and generation retirement stop recurring demand.
Regression coverage includes 120 consecutive 60 Hz cells, early estimated
presentation, off-grid requests, an already armed animation superseded by input,
and hidden membership completion without continued polling.

The combined source passed 605 workspace all-target tests, with zero failures
and one existing ignored isolation test, plus workspace all-target Clippy with
warnings denied. Evidence: `/tmp/brood-pure-workspace-final2-tests.log` and
`/tmp/brood-pure-workspace-final2-clippy.log`. The release/native verification
of these last scheduling and resize changes remains pending.

Release `8022cc1398676512377a3ce93ae802b260e457b0cfe0b94a1696ebcdf249a546`
with desired-opportunity scheduling passed every 60 Hz criterion in
`/tmp/brood-render-pure-gate-cadence/render-scheduling-report.json`: unsaturated
1121 measured opportunities, key p50/p95 11.604/18.931 ms, frame p95 16.605 ms;
saturated key p95 20.510 ms and frame p95 16.775 ms. Protocol, actor, and renderer
errors were zero. Overall acceptance still failed the 120 Hz opportunity count:
the example explicitly requested 16.667 ms intervals. Its continuous animation
now requests the next host opportunity, independent of refresh rate; five pure
example tests, Clippy, and component validation passed. Full gate rerun pending.

The shell42 post-close deadlock was transport backpressure, not missing guest
publication. Ordinary snapshot 402 omitted closed pane 3 while the retained
accepted layout still referenced its surface. Removing that backing prevented
queue.present/acknowledgement of 402, which blocked the backend from exporting
the exact resized snapshot needed for the replacement layout. The host now
retains omitted accepted-layout members until atomic layout withdrawal, then
releases unreferenced backing. A pre-catalogue newly arrived scene is not treated
as withdrawn. The focused regression and client Clippy passed; independent
review found no blocking defect. Native shell verification remains pending.

The shell43 run passed managed close, populated diff rendering, real scrollbar
drag, fractional scrolling, and diff close initiation; the populated screenshot
was visually reviewed. It then exposed client termination on a retired facet
generation (`Error: facet actor generation unavailable`), rather than a failed
worktree action. Retirement handling must remove pending scheduling/input
authority without propagating expected stale-generation work as a fatal client
error. The shell gate remains unaccepted.

`/tmp/brood-render-pure-gate-refresh` again passes every 60 Hz criterion (key p50
10.922 ms unsaturated). The 120 Hz lane has 905 opportunities and frame p95/p99
8.355/8.730 ms, but its aggregate 67.464 fps fails the 114 fps requirement.
The apparent rate was a measurement-window defect, not half-rate presentation.
The chronological interval tail contains 516,569,991 ns and 5,500,354,544 ns
samples created by the harness's explicit subscription-settle and F12-to-F11
idle-control phases. The report assembler summed those idle intervals into the
direct-lane FPS denominator while the active distribution remained at 120 Hz.

The harness now sends a first F12 at the explicit end of active measurement,
before screenshot capture and subscription shutdown. The client freezes the
active interval prefix at that first trigger. A second F12 after shutdown has
settled replaces only the idle-counter baseline; it cannot overwrite the frozen
active prefix, and F11 still finalizes the independent idle interval. The report
requires the frozen samples to be a chronological prefix of the final samples
and computes direct FPS and interval percentiles from that exact active window.
The unchanged minimum of 599 intervals therefore applies to the active window,
preventing a short high-rate prefix from satisfying the gate. No duration-based
outlier filtering or acceptance-threshold change was introduced; a legitimate
40 ms frame inside the active prefix remains in its FPS denominator.

The report parser regressions pass 3/3, covering suffix exclusion, missing and
misordered boundaries, and retention of a slow active frame. The analyzer
self-test also rejects a 598-interval active window. The Nix-backed focused
client check, harness shell syntax check, and diff check passed. Fresh native
acceptance remains pending because the existing refresh artifact predates the
explicit boundary and correctly lacks the required evidence.

### Retirement and final gate review — 2026-09-06

Actor retirement now revokes scheduler authority when the actor leaves the
runtime map and explicitly detaches bridge-owned scenes to break the
scene/resource/backend/host reference cycle. Every compiled pure scene retains
its exact budgeted CPU snapshot; closing during asynchronous preparation never
substitutes a newer bridge publication. Recovery carries those snapshots across
the device replacement, prefers the accepted scene, and releases temporary
replay-map entries after compilation. Retained withdrawn panes have no input
authority, including an existing pointer grab. The native texture and its CPU
lease remain only until atomic layout withdrawal. Generation-scoped cleanup
and budget-pin regressions pass; an actual bridge/host-cycle unit fixture is
not yet present because the bridge requires a Winit event proxy.

Release `958ff61bce1144f653f43733659f82816079855d8e693100a00c3609e851d4b9`
passed Foot open/close, populated diff drag/scroll/close, and worktree creation in
`/tmp/brood-shell-pure-final45`. The run stopped on verbose CLI logging mixed
with the harness's JSON response; the control helper now fixes its log level.
Visual review also caught the close screenshot being taken during the retained
old layout. The oracle now waits for completed withdrawal before capture.
The full shell gate remains pending.

`/tmp/brood-shell-pure-final44` instead timed out waiting for Foot's initial
exact buffer. That run lacked Foot protocol and backend info logs, so the cause
is not established. The traced next run shows configure, ACK, exact buffer,
and atomic two-pane readiness in about 10 ms. No timeout was increased.

`/tmp/brood-render-pure-gate-active` stopped during hidden-pane restoration.
A native frame became geometrically stale while newer membership demand was
pending; discarding the accepted frame did not wake scheduling. The cancellation
path now wakes the event loop, and a regression proves newer membership demand
remains dispatchable. Client all-target Clippy passes. Fresh native acceptance
and default rollout are still pending.

The full shell gate passes at `/tmp/brood-shell-pure-final48/proof.md` with
release `9c5996fecb672c5f561a3b6afae606f93302f58205de5210ce0b2bdff433d2dc`.
It covers actual managed open/close (61/51 ms), 16 native diff drag commits,
40 fractional scroll events settling in 431 ms, diff retirement, worktree
creation/reuse, the same diff actor surviving a session round trip, and two
project groups. Screenshots were reviewed, including actual diff withdrawal
and the populated project list. The preceding shell46/47 failures were invalid
input timing/focus expectations: the harness typed before a new picker view
settled, then required a new focus event for an already focused pane. It now
waits for native layout/current picker readiness and verifies current focus at
the pane's actual layout rectangle. It retains all semantic and visual checks.

The complete active2 rendering run passed key latency and 120 Hz cadence
(122.936 fps), with zero actor/renderer/protocol errors, but failed the
unsaturated deadline rate at 12/1133 against 1%. Its final counters included
post-active control work; the old artifact cannot identify which misses were
inside that interval. Cancellation alone does not count as a deadline miss.
The first-F12 snapshot now stores intervals, opportunities, deadline misses,
and target misses together for every lane. Tests preserve genuine active misses
and reject active counters exceeding final totals. No threshold changed;
active3 native verification is pending.

Active3 completed with zero actor/renderer/protocol errors and passing latency,
frame cadence, and 120 Hz checks, but the correctly bounded active interval
still had 25 deadline misses out of 1097 opportunities (limit 1%). Median/p95
key latency were 11.073/18.220 ms and 120 Hz measured 123.291 fps. This proves
the measurement-scope correction alone does not resolve performance acceptance.
Default rollout remains held. Bounded late-finish records are being added to
separate host wake delay from execution-cost underestimation before changing
scheduler behavior.

The miss-only diagnostic run
`/tmp/brood-render-pure-gate-late-diagnostics` captured all 15 active deadline
misses (1096 opportunities, zero diagnostic overflow). Seven followed F9
renderer recovery: dispatch was 2.740–5.364 ms late while execution took only
6–984 us. Six later steady-state misses had normal 71–163 us dispatch delay
but execution spikes of 771–2018 us against 350–695 us estimates and the
500 us safety floor. Two remaining misses were small mixed cases. Visibility
toggles occurred after the recovery burst. Composition-barrier delay was zero.

The predictor doubles slack on a miss but previously decayed it by 31/32 on
every success. Under persistent jitter above the floor, that controller's
steady miss rate is about 4.38%, incompatible with the 1% acceptance target.
The planned correction retains the same doubling and 0.5–4 ms bounds while
using 255/256 success decay (about 0.55% equilibrium). A deterministic sustained
wake-jitter workload and bounded transient recovery must verify this before
another native gate. Acceptance thresholds remain unchanged.

The 255/256 decay candidate was rejected after native verification in
`/tmp/brood-render-pure-gate-jitter-retention`: deadline misses passed at
8/1096 (0.73%), but median/p95 input latency rose to 13.841/20.929 ms,
exceeding the unchanged 12/20 ms limits. Warmup wake spikes left roughly
2–3.5 ms of early dispatch margin across the input workload. All lifecycle,
error, and 120 Hz checks passed. A bounded estimator using measured timing
magnitudes is being evaluated; the boolean controller tradeoff is not an
accepted solution, and defaults remain off. The target still permits only
one facet invocation per predicted opportunity; extra same-opportunity
rerenders are not an authorized workaround.

Wake-path review found a separate concrete race: after the deadline worker
fired, redraw could synchronize the same deadline before its queued Winit
event was processed. Because the pending state stored only a generation,
`replace` invalidated that valid event and rearmed the already-past deadline.
The pending state now retains its deadline as well as generation; synchronizing
the same deadline is idempotent through event acceptance. Different deadlines
and cancellation still invalidate the old wake. A regression exercises repeated
redraw synchronization after firing and cancellation of a queued wake. This
fix removes an unnecessary worker round trip; native impact remains to be
measured rather than inferred from earlier miss-only records.

The candidate replaces boolean feedback with a bounded 256-observation wake
tail, retaining the second-largest sample and the existing 0.5–4 ms bounds.
Actor execution separately uses an EWMA plus its own positive-residual tail;
expensive guest work cannot inflate peer wake margins. Residual history starts
with zero rather than repeating a cold first invocation. Recovery reanchors
shared wake history while preserving actor execution history. The finite-rank
statistical rationale assumes exchangeable observations and is not a native
latency guarantee. Known composition-barrier wait is subtracted before shared
wake feedback. Client tests pass (336), including bounded aging and the queued
wake regression. The synthetic cost-retarget test now uses fixed 5 ms spikes;
its former cost-of-current-estimate-plus-one recursively increased the workload
itself instead of testing a stable workload. Native acceptance remains pending.

Native tail-estimator validation did not pass. The first run
`/tmp/brood-render-pure-gate-tail-estimator` stalled on pane restoration after
stale-geometry discard (candidate353), before a full timing report. A second
instrumented run `/tmp/brood-render-pure-gate-restore-diagnosis` completed all
lifecycle lanes but failed input latency at 15.674/23.461 ms median/p95; deadline
misses were 7/1096 and direct120Hz measured123.363fps. The intermittent restore
stall remains unresolved, and the passing lifecycle repetition is not treated
as its fix. Read-only review excluded receipt retention in the ordinary discard
path. A proposed missing visibility wake was rejected on review because
`about_to_wait` immediately services scheduling after publishing visibility.
Bounded full timing observations are being added to distinguish retained
startup wake margin from actor execution lead before further estimator edits.

Full timing observations in `/tmp/brood-render-pure-gate-full-timing` identify
why the 256-sample wake tail still fails latency: token1 had a23.492ms wake
delay, and token67 had2.438ms, while median ordinary wake delay was64–82us.
Those two samples pinned a2.438ms wake margin across the key workload. The
report failed at13.137/20.251ms median/p95; active misses were8/1099.
The next candidate uses512 shared wake samples and finite-rank508, allowing
four rare observations above the estimated lead (5/513 predictive exceedance
under exchangeability). Actor residual history remains256samples. Offline
replay of the observed timings predicts a500us key-workload margin while
remaining within the active miss budget; this is a counterfactual estimate,
not native acceptance. All336client tests pass for the candidate.

The wider-tail run reproduced restoration failure with useful diagnostics:
`/tmp/brood-render-pure-gate-wider-tail` candidate358 was accepted for epoch3,
then failed preflight after asynchronous native preparation. Cancellation left
newer demand (requested1234/satisfied1233); the receipt discard was observed
with neither native work nor invocation pending. Tokens359–379 were consumed
without starting a facet, and token380 started after epoch abort. The only
unlogged admission branch matching this evidence is resource `LimitReached`.

Native preparation caches `PreparedCanvasScene`, which owns the source snapshot
and its facet resource lease. Stale preflight returned before consuming that
cache. Retaining a rejected frame could therefore exhaust the four invocation
slots and prevent a replacement from superseding it. Exact rejected pane/
sequence/epoch keys now retire cached scenes, pending keys, and latest-key
authority; late worker completions are rejected, while newer and peer work is
preserved. A cache ownership regression proves resource release and late-result
rejection. Separately, ordinary receipt promotion failure and empty deferred
facet commits now return/cancel their accepted tokens; replayed missing frames
do not cancel a valid pending presentation. Client338tests and all-target
Clippy pass. Native stress verification is next; invocation bounds stay unchanged.

A target-compliance regression also proves a completed frame cannot consume
the same predicted opportunity twice after cancellation. The scheduler now
retains one completed-frame target floor per actor generation, independently
of its pending native presentation. Pure frame completion sets the floor; an
empty admission decline does not. Cancellation before targetT must schedule
the replacement afterT; a new actor generation starts fresh. Client339tests
and Clippy pass; releaseclient SHA
`7bd7ebd61c255d8a8f71d6610d62cad135053e21615bd9a434a6f031cf6ecc8f`
contains the preparation, promotion, and opportunity-floor corrections.

The opt-in stress reproducer `/tmp/test-render-stale-retry.sh` completed all
40 visibility transitions in `/tmp/brood-render-stale-retry-final1`. Each
transition required a newer bridge acceptance with expected visibility and an
exact two-pane native commit. Six actual stale-geometry collisions recovered;
there were1,324ordinary two-pane commits and no visual facet failure. This
validates progress under the formerly failing overlap without raising resource
limits. The modified run intentionally adds40transitions and fails the normal
2%target-miss limit, so it is not timing acceptance. The unchanged strict gate
is being rerun at `/tmp/brood-render-pure-gate-retirement-final`.

The unchanged gate `/tmp/brood-render-pure-gate-retirement-final` passed
latency, deadline/target misses, cadence, visibility, recovery, and error checks.
Unsaturated median/p95input10.903/18.150ms, deadlines4/1092, targets10/1092;
saturated deadlines23/3189 and p95input23.238ms; direct120Hz123.489fps.
It still failed the saturated Winit callback maximum:15.368ms against10ms
(p991.555ms). Narrow slow-callback identity and threadCPUtime diagnostics
are being added to distinguish event-loop work from blocking/descheduling.
No callback or other acceptance threshold has changed; defaults remain off.

The callback diagnostic run `/tmp/brood-render-pure-gate-hot-callback` again
failed only the Winit maximum. Its two slow saturated callbacks were redraws:
8.774mswall/8.628msCPU and12.577mswall/12.441msCPU, with only48/54usdriver
presentation. Thus OS descheduling does not explain this run's tail; redraw
CPU work is the remaining acceptance blocker. Existing acquire/encode/submit
timings are now attached to slow-callback records. A saturated-only diagnostic
uses `/tmp/test-render-callback-saturated.sh`, derived from the unchanged
offscreen harness; it is diagnostic evidence, not a complete acceptance run.

Saturated-only redraw breakdown captured5.083mswall, with4.673msencoding,
277ussubmission and86usacquisition. The complex canvas path opened a separate
MSAA render pass and rebuilt the same stencil for each DrawOp. It now batches
consecutive non-advanced draws with identical clips on the same target, across
source/pipeline changes. Order is preserved; layer boundaries and each
destination-dependent advanced blend remain separate. Simple-scene encoding,
MSAA sample count, clip geometry, and blend fidelity are unchanged. No extra
staging allocation is introduced. Eight GPU tests and all-target Clippy pass;
fresh native timing and visual comparison are pending.


The post-batching run `/tmp/brood-render-pure-gate-gpu-batching` passed all
latency, deadline, cadence, and hot-path limits: Winit maximum 4.598 ms and
p99 1.266 ms; unsaturated input median/p95 11.406/18.327 ms with 3/1096
deadline misses; direct 120 Hz lane 123.911 fps. Fixed screenshot regions
are pixel-identical to the pre-batching capture after excluding the animated
squares and terminal text containing the changed temporary path. The full
gate still failed idle: two render turns, two deadline wakes, and four redraws.
The actual second idle baseline is followed by a leaked F12 release and empty
facet inputs; investigation also covers virtual keyboard device lifecycle at
the measurement boundaries. Defaults remain off pending a valid zero-work
idle interval and actual assembled-package verification.


The idle proof now consumes both edges of its F9–F12 control keys and uses one
virtual keyboard for both idle boundaries. Previously F12 release reached the
plugin after the baseline, and constructing/destructing separate keyboards
introduced additional input/reset traffic. The replacement helper sets up its
device before the baseline, leaves it alive throughout 5.5 seconds, and finalizes
before teardown. No idle counters or acceptance thresholds are relaxed, and
ordinary product input is unchanged. Native verification is pending.


### Default rollout — 2026-09-06

The complete strict gate passes at `/tmp/brood-render-pure-gate-idle-fixed`.
The 5.5-second idle interval has zero hints, actor render turns, deadline wakes,
redraw requests, and periodic polls. Client tests pass (340), as do all-target
Clippy and release compilation. All ten visual plugins and the graphics example
now default to `pure-renderer`; explicit legacy builds use
`--no-default-features`. The SDK does not globally enable facets. Test harness
selectors follow the new default while preserving explicit legacy selection.
Actual assembled Nix package construction and final package verification follow.


Final packaging uses `nix build path:.#brood` from
`/tmp/brood-pure-final-source`, a 577-file source snapshot including all tracked
and nonignored untracked implementation files. SHA-256 hashes are recorded in
`/tmp/brood-pure-final-source-manifest.json`. Building the original checkout via
`path:.` copied ignored local build caches before source filtering, so that copy
was interrupted before compilation. The source snapshot avoids those caches
without omitting the new modules. The bundled coverage audit includes all
17 plugins in 10 native lanes; all four report-assembly tests pass.

When injecting a prebuilt package into a harness, its artifacts determine the
renderer mode. Selecting legacy assertions does not transform a pure package;
legacy verification requires artifacts built with `--no-default-features`.


The assembled package `/nix/store/x2y8lq23c7jgavbmximz3l62hx5hqq8r-brood`
built successfully and validates all 11 facet exports, seven required sidecars,
package root, binaries, and Neovim runtime. Its native run
`/tmp/brood-render-pure-gate-package-final` passes idle, latency, deadline,
visibility, and recovery checks, but has two encoding CPU spikes: about 9.8 ms
encoding and 10.2 ms callbacks, just above the unchanged 10 ms maximum.
This package is not final acceptance. Narrow encoding-stage diagnostics are
being added to locate the remaining work; the default rollout remains under
verification and no completed rollout is claimed.


The instrumented Nix diagnostic `/tmp/brood-render-pure-encode-nix` localizes
the residual spike to `encoder.finish()`: pane command recording takes
43–92 us, composition 1–9 us, and finalization up to 11.259 ms. Wgpu 30 defers
command validation/encoding until finalization, so the earlier per-pane timing
alone could not locate this cost. The five-pane scene records approximately
47 offscreen passes. Initial clears now fold into the next ordinary draw batch
when safe, removing approximately seven passes/resolves in this workload.
Advanced-first and empty targets retain explicit clears. Nine GPU tests and
client all-target Clippy pass; a rebuilt package is under native verification.
The threshold remains unchanged.


The clear-fold package `/nix/store/ign71fyyk7bsw4b8fgqw3a12cmvh3crs-brood`
passes visual equivalence (zero fixed-region pixel differences) and the callback
limit (maximum 8.321 ms), but `/tmp/brood-render-pure-gate-clear-fold` reports
unsaturated median input latency 12.424 ms against 12 ms. Actual guest and wake
costs did not regress. The original 260-key burst spans only about 18 refresh
cycles; the fixture now uses 2,080 keys and explicitly checks that the sample
set is complete and below the 4,096-entry history bound. All timing thresholds
remain unchanged; the prior failure remains recorded rather than discarded.

Fresh coding-agent verification exposed test setup assumptions. Counting only
ordinary commits missed synchronized pure startup. Merely accepting an earlier
quiescent layout was also too early: Wayland traces show virtual device
capability addition/removal queued while later startup transactions were still
being processed, before Winit could bind the devices. Startup now waits for the
actual terminal/sidebar frame at the fixture's exact geometry. Real input and
clipboard attachment then progressed. The GTK drag source inherited the host's
`GDK_BACKEND=x11`; it now explicitly uses Wayland inside the test compositor.
Failure captures and command diagnostics are preserved to make future failures
reviewable. These are harness corrections, not relaxed product assertions.

The larger input workload required extending the bounded Winit callback history
from 65,536 to 131,072 entries. Report assembly continues to reject truncated
samples. Immutable image backing reuse now has independent 64-entry, 16 MiB
CPU, and 16 MiB GPU limits, with off-thread dimension-aware hashing and exact
byte comparison before sharing. Guest handles and lease charges remain separate.
All 345 client tests passed; after a Clippy correction, 13 GPU tests and client
all-target Clippy passed (`/tmp/brood-pure-image-cache-check2.log`).

The rebuilt `/nix/store/9qv4y580cv3z7y5vdrqmn823n21cmh5d-brood` passes every
strict gate except the callback maximum in
`/tmp/brood-render-pure-gate-image-cache`: unsaturated input median 11.163 ms,
p95 18.340 ms, saturated p95 22.553 ms, and zero idle activity. One callback
reached 10.257 ms (9.923 ms thread CPU, 9.212 ms encoding). Image backing reuse
records 5,069 hits and one miss. Perf diagnostic
`/tmp/brood-pure-encoder-perf.data` records no lost samples and shows RADV
render-pass ending, MSAA resolves, command-pool resets, and allocator activity
on the main thread. This supports further pass reduction; it does not make the
remaining callback failure acceptable or change the target.

Fresh coding-agent proof passes at
`/tmp/brood-pure-final-bundled-offline2/coding-agent/proof.md`, including real
input, two image attachments, worker execution, image cleanup, completion inbox,
and navigation. The live provider failed with HTTP 401 refreshing login, so this
run explicitly uses `BROOD_CODING_AGENT_OFFLINE=1`. Its fixture validates the
real worker request and exact solid red/green attachments before writing proof
files; it does not establish live model/provider functionality. The normal live
path remains available. The fresh editor lane then exposed one remaining
legacy-only initial draw counter despite committed pure frames. Its initial
readiness now requires a native frame for the editor's exact pane, through
either acceptance path, rather than unrelated global legacy draw traffic.

Ordinary GPU draws now retain one pass across clip transitions. A stencil-only
Equal/DecrementClamp pipeline unwinds the old suffix in reverse; the exact common
geometry prefix remains, then the new suffix increments normally. Advanced
blends and layer boundaries remain separate. Changed stacks at 255 or more
clips conservatively retain the previous pass boundary. Fifteen GPU tests pass,
including exhaustive per-sample membership checks; client all-target Clippy passes.
Fixed-region comparison against the clear-fold native capture has zero differing
pixels (only the five animated squares and native terminal column are masked).

Package `/nix/store/3mj9xkga8jlx268479sdffn8hdqc0idc-brood` passes the timing
analyzer in `/tmp/brood-render-pure-gate-clip-transition`, but the separate input
completeness assertion detects 2,078 saturated samples for 2,080 actual presses.
All 2,080 presses appear in the Winit log. Investigation identified accepted
frame token 877 canceled for stale geometry while newer render demand remained:
`cancel_accepted` settled its trace without returning its already-reduced input
correlations to the pending queue. The missing records are a scheduler accounting
bug, not justification to lower the sample assertion. A bounded restoration and
exactly-once replacement-presentation regression are being implemented.

The fresh editor workload passes at
`/tmp/brood-pure-final-editor-clip2/editor/proof.json`, including local Wasm
Neovim, host Lua configuration, remote open/save, large-file chunking, and
terminal/diff/agent routes. The populated diff screenshot was visually reviewed.
Its CLI calls now use warning-level logs so informational startup output cannot
contaminate structured status JSON; runtime tracing remains enabled. The default
proof also explicitly requires a pure native receipt for the editor actor.

Accepted cancellation now restores input correlations to the bounded ready queue,
or transfers them directly to an already-outstanding newer causal frame. Tests
cover both interleavings, the retained next-opportunity target floor, repeated
cancellation, post-presentation cancellation, and retirement. The broader test
run also exposed a fixture lifetime race in the legacy concurrent-mailbox test:
the outbox task owned the last registry reference and could retire the mailbox
before the host producer finished. The fixture now retains registry ownership
through both producers and final consumption, and requires a clean empty mailbox
rather than accepting an arbitrary final read error. This changes test lifetime,
not production mailbox retirement behavior.

Strict packaged acceptance now passes in full at
`/tmp/brood-render-pure-gate-correlation-final/render-scheduling-report.json`
using `/nix/store/gsrhciv9pnw1gixyjz8vmw97ks2354q8-brood`. It retains 2,081
unsaturated and 2,084 saturated key samples, Winit maximum 6.078 ms/p99 0.548 ms,
unsaturated key median 10.775 ms/p95 17.951 ms, saturated p95 21.725 ms,
123.564 fps direct cadence, and all-zero 5.5-second idle counters. Actor/renderer/
protocol failures and reliable transition drops are zero. Workspace tests pass
628 with one pre-existing ignored systemd-isolation test; all-target Clippy passes.

Fresh package lanes pass at `/tmp/brood-pure-final-remaining3`: narrow keybindings,
runtime I/O, resize, complete session shell, and empty startup. Workspace-watch
and worktree pass at `/tmp/brood-pure-final-remaining4`. The shell prebuilt fixture
needed to copy backend/session roots and sidecars from their actual installed
locations, in addition to the separate client package; all required artifacts
were already installed. The actual runtime source remains unchanged by that
harness correction.

Completion remains blocked by a newly reproduced terminal correctness failure:
`/tmp/brood-pure-final-remaining3/terminal/pure-paste.bin` contains only 163,834
of the expected 262,144 quote/backslash bytes. The real paste shortcut arrived.
Terminal session `queue_input` accepts only the prefix fitting its 64 KiB pending
buffer and silently loses the rest at the protocol boundary, recording a local
error. Pure paste semantic chunks can arrive faster than the PTY drains. Earlier
successful paste runs did not expose this timing-dependent overflow. The next
change must preserve bounded storage and reliable byte order rather than merely
increasing the cap or weakening the exact paste assertion.

Terminal input now uses private B/C stream negotiation and J/K written-chunk
acknowledgments. The UI atomically admits a logical input into a bound derived
from one maximum 256 KiB paste, bracket delimiters, and the existing 64 KiB
interactive reserve. It sends one 32 KiB chunk at a time and retains following
keys in FIFO order. The backend reserves that one chunk alongside legacy input,
never acknowledges a partial write, and advances visible snapshot causality only
at the final chunk of a logical input. Stream incarnation and logical sequence
base prevent old acknowledgments from releasing new input after reattachment.
Legacy I records now reject whole records instead of accepting prefixes; overload
produces a bounded visible inbox error. No host ABI or pure-renderer target was
changed. The host recognizes only the first J chunk as a new input correlation.

Independent review checked duplicate acknowledgments, START/LAST ordering,
reconnect draining, and writable-notification delivery. Sender tests cover a
256 KiB paste followed by a key, single in-flight operation, capacity rollback,
and stale controls. Receiver tests cover partial writes, duplicate records before
and after completion, logical continuity, and atomic overflow. The native proof
now pauses the raw PTY reader for five seconds, queues the real paste and then a
real z key, and requires exactly 262,144 paste bytes followed by exactly that key.
This deliberately exercises backpressure instead of depending on drain timing.

The final terminal test run exposed a native fixture race: three tests mutated
the same Wasm actor singleton concurrently. A test-only mutex now covers each
complete fixture lifetime, with cleanup after each test. The corrected run
passes 26 pure UI, 13 legacy UI, and 20 session tests
(`/tmp/brood-terminal-reliable-final-tests2.log`). The full workspace test and
all-target Clippy run also passes after the host J-message correlation update
(`/tmp/brood-pure-workspace-final5.log`). Native backpressure acceptance still
requires the newly rebuilt package.

The actual rebuilt package
`/nix/store/c58hmbd76c7adarxf5v50bgjfinidp3s-brood` passes component validation
for all eleven visual facet producers and seven supporting modules
(`/tmp/brood-pure-reliable-package-verify.log`). Its strengthened real terminal
workload passes at `/tmp/brood-pure-reliable-native1/terminal`: while the raw PTY
reader was paused, the real paste shortcut queued 262,144 bytes and a following
real z key. Exact byte comparison confirms the entire paste followed by exactly
that key. The styled Unicode screenshot was visually inspected. This closes
the reproduced truncation blocker; strict timing is being rerun against the
same package.

## Final packaged timing acceptance

The same c58hmbd package passes the complete strict gate without threshold
changes: `/tmp/brood-render-pure-reliable-final.log`. A persistent copy of its
report is [[plugin-pure-renderer-final-report.json]]. Normal load retains all
2,081 causal samples (p50 11.023 ms, p95 18.239 ms, p99 18.915 ms, max
20.194 ms). Saturated load retains all 2,084 samples (p95 22.178 ms,
p99 23.242 ms, max 30.185 ms). Winit retains all 81,023 callback samples
(p99 0.465 ms, max 6.518 ms); Smithay p99 is 0.069 ms, max 2.141 ms.
The direct lane measures 123.993 fps over 1,089 opportunities. The 5.5-second
idle interval records zero polls, render turns, hints, or scheduler wakes.
Actor failures, renderer failures, protocol errors, reliable transition drops,
and partial actor-frame promotions are all zero. Captures verify hide/show,
recovery, saturation, and direct presentation. Presentation timestamps remain
queue/present-return estimates rather than physical scanout measurements.

Final affected-workflow reruns also pass: editor, coding-agent, and populated
session shell at `/tmp/brood-pure-reliable-workflows`. Editor and shell use the
same c58hmbd Nix package. Coding-agent rebuilds the current local host and
components through Nix, as its harness requires. Editor verifies native facets
with zero legacy draw packets, real open/edit/save of three files including a
119,001-byte stream, host Lua configuration, and terminal/diff/agent routes.
The populated shell covers diff scrolling, project/worktree navigation, closure,
and Ctrl+O layout (110 ms). Editor and agent completion captures were visually
inspected.

Coding-agent uses the explicit deterministic offline worker fixture. Its real UI,
worker dispatch, clipboard and drag/drop PNG transfer, cleanup, inbox click, and
post-completion configuration all pass. This does not validate live model service
access: the earlier live-provider run returned HTTP 401 while refreshing login.
That external authentication limitation remains disclosed rather than being
reported as a successful provider run.

The unaffected native lanes retain the immediately preceding passing evidence:
keybindings-narrow, runtime-io, resize, and startup-empty under
`/tmp/brood-pure-final-remaining3`; workspace-watch and worktree under
`/tmp/brood-pure-final-remaining4`. Together with the four final affected lanes,
all ten coverage-matrix lanes have passing evidence for the migration. No
target was rewritten to accommodate implementation failures.

Final completion checkpoint: `cargo test --workspace --all-targets` through Nix
passes 628 tests, with zero failures and one existing user-systemd isolation
ignore (`/tmp/brood-pure-workspace-all-targets-final.log`). All-target workspace
Clippy passes, as do the terminal pure/legacy/session suites and WASM Clippy.
The packaged source SHA manifest matches the final non-documentation source.
`git diff --check` is clean. All ten visual plugins and the graphics example
default to pure facets, the final actual package and strict gate pass, and the
reproduced timing/correlation and terminal truncation blockers are closed.
Implementation is complete; live-provider authentication remains the explicitly
bounded external verification limitation above. Changes are left uncommitted.

## September 8: production drag regressions reopened

The new real continuous drag matrix invalidates the earlier assumption that
passing the resize proof and timing gate covered production divider behavior.
Session-switcher root previews deadlock on an intentionally omitted child-region
response, then cancel capture. Workspace/diff drag epochs can repeatedly become
ready and be superseded before presentation, leaving only endpoint movement.
The test setup, causal evidence, all-plugin visual coverage, and retained
captures are recorded in [[plugin-drag-resize-regressions]]. The new
`divider-drags` bundled lane currently fails and must remain failing until
these regressions are repaired. The pure renderer target remains unchanged.

## September 9: production drag regressions repaired

The changed child-region discovery omission and workspace candidate
supersession starvation are repaired without relaxing exact geometry or
changing the target. The actual rebuilt package passes all 18 real drag
gestures across all ten visual plugins. Every plugin's intermediate captures
were visually reviewed. Workspace all-target testing passes 628 tests (one
existing ignore), Clippy passes, and the unchanged strict packaged rendering
gate passes. The final implementation details, failure-state tests, and
before/after evidence are in [[plugin-drag-resize-regressions]].
