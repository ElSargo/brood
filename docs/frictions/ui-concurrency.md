---
status: friction
type: design-input
authority: implementation
---

# UI Concurrency Frictions

## Scope

This audit compares the current native client and first-party UI plugins with
the [[../targets/systems/ui-concurrency]] target. It records ordering failures
and structural hazards visible in the implementation on 2026-08-22. It does
not assume that two threads must write the same memory for a race to exist; an
old asynchronous result changing newer logical state is also a race.

The common failure is incomplete causal identity. Several paths have good
individual mechanisms—connection generations, actor generations, layout
leases, inventory revisions, resize epochs, retained draw sequences—but those
mechanisms are not consistently carried through user intent, persistence,
catalogue, platform-service, and cross-lane transitions.

## Confirmed stale-result mutations

### Clipboard selection readers can overwrite a newer selection — remitted

Offer reads now stamp a selection generation via `begin_offer_epoch` /
`clear_clipboard_slot` / `set_clipboard_slot`. `spawn_offer_reader` commits
through `commit_clipboard_slot`, which ignores results whose generation is no
longer current, so a slow read for selection A cannot replace selection B or an
intervening `set_text` (`clients/brood-client/src/clipboard.rs`). Superseded
reader threads may still finish their pipe I/O; they simply drop the result.
Covered by unit tests in the same module.

### Session command completions mutate unrelated newer state — remitted

`apply_completion` now requires `pending == Some(completion.request)` before
clearing pending or mutating error, status, selection, query, edit mode, or
activate/create realm identity—the same gate as directory-list and worktree
completions. Unmatched or late replies are no-ops and leave a still-pending
request in place for the real reply. Covered by unit tests in
`plugins/session-switcher/src/lib.rs`.

### Keybinding save replies survive cancellation — remitted

Closing the keybinding editor removes `pending_save`. `handle_saved` now
requires a matched pending action before either the success or failure path
mutates UI state; mismatched or missing pending replies are no-ops and leave a
still-pending save in place for the real reply. Covered by unit tests in
`plugins/keybindings/src/lib.rs`. The reply still names an action rather than a
unique save-attempt id; that stronger correlation remains future work if
overlapping saves for the same action become possible.

### Repeated backend-picker creation reuses one correlation value

Every discovery picker uses the constant request value `backend-discovery`.
Starting again clears the known picker identity, while any later reply with
that constant becomes the current picker
(`plugins/session-switcher/src/lib.rs:2464-2499,2908-2919`). A late reply from
an earlier open can replace or update the newer picker. More generally, picker
clients cannot cancel an in-flight creation before the service returns a
picker identity, so a late create reply can resurrect a closed modal.
The keybinding plugin now rejects a late `picker-created` when
`picker_requested` is false (after close/cancel) and sends `picker-cancel` for
the orphaned identity. Session-switcher discovery pickers still have the
broader correlation hole.

### Agent-picker intent changes realm while it waits — remitted

`agent_picker_pending` is now scoped to `(backend_id, session_id)`. Realm
changes (Snapshot, layout-state focus, activate/create completion, and
worktree completion) clear agents and the pending intent via
`clear_agent_registry_for_realm_change`. Registry fulfillment only opens UI
when the pending realm matches the current realm
(`plugins/session-switcher/src/lib.rs`). Covered by
`agent_picker_pending_must_not_fulfill_after_realm_change`.

### Layout completions can move durable state backward — remitted

Completed and degraded layout notifications still always retire their pending
entry, but they install `durable_shell_layout` and advance
`presented_layout_revision` only when `request_id` is at least the last
committed layout request. Presented revision is monotonic via `max`, so a late
older completion cannot restore stale shell chrome or rewind the presented
revision (`plugins/session-switcher/src/lib.rs`). Covered by
`late_older_layout_completion_must_not_regress_durable_or_presented_revision`.

## Authority and publication races

### Session and backend catalogues can regress each other — remitted

Remitted by revision gates on snapshot apply and catalogue apply in the
session switcher, plus optional `snapshot_revision` stamps on
`BackendCatalogueEntry` and a client-map reject for older
`SessionSnapshot.revision` values. Empty-row preservation remains only as a
bootstrap aid when no revision has been observed yet. Residual risk: pane /
route / renderer state from a delayed `BackendSnapshot` event is still applied
before the session-map gate; only the session catalogue / actor broadcast path
is revision-gated today.

### Command completion and catalogue publication are independent

The local command worker posts completion as soon as the DBus call returns
(`clients/brood-client/src/main.rs:2400-2435`). The server publishes the
catalogue after it has handled and replied to the DBus request
(`crates/broodd/src/control.rs:2938-2965`). The native client responds to a
successful activate or create by selecting the session immediately, before a
catalogue containing that identity is guaranteed to be installed
(`clients/brood-client/src/main.rs:9549-9577`). The worktree flow already has a
special-case workaround, showing that this ordering cannot be assumed. A
commit revision would remove the need for per-operation workarounds.

### Backend deployment results do not identify the initiating intent — remitted

Remitted by threading an optional deploy `request` id from the session switcher
through host deploy messages, deployer ready/failed client events, and
`BackendCatalogueMessage::ConnectionFailed`. The switcher ignores failures that
do not match the current `pending` id and clears `pending` only on a match.
Residual risk: in-flight deployer threads are not cancelled when superseded; they
are only ignored at notification time. Ready/success paths still do not clear
`pending` by request id.

### Critical and latest-state input lack a shared source watermark

Remote critical input and latest-state input are sent by independent tasks
(`clients/brood-client/src/remote.rs:451-493`) and merged later by another
task (`crates/brood-remote/src/main.rs:1495-1523`). The merge assigns sequence
only after the lanes have raced. Events such as button input include position
and mitigate ordinary click ordering, but a delayed pointer sample can still
cross a pointer-leave, focus, backend-authority, or connection transition and
revive obsolete hover or targeting state. Priority selection is not a causal
barrier.

### Focus transition pieces use independently pressured paths

One focus change sends a keyboard reset through the backend input lane,
plugin blur and focus through actor presentation events, and managed-window
focus through input again (`clients/brood-client/src/main.rs:12004-12041`). The
renderer has already chosen the new focus, but the consumers have no common
focus epoch. Backpressure can expose transient old focus or let later input
overtake part of the transition.

### Several retained queues are bounded only by debug assertions

Local actor messages and input, remote critical input, and remote actor
messages append to retained deques after only `debug_assert!` capacity checks
(`clients/brood-client/src/main.rs:2020-2023,2158-2170`;
`clients/brood-client/src/remote.rs:1236-1243,1314-1317`). Release builds can
grow beyond the named limit. Sustained pressure becomes memory growth followed
by replay of a large stale tail, making timing failures worse and violating
the advertised bounded-flow-control contract.

## Native event-loop stalls

### Backend teardown blocks the presentation loop

Remote reaping polls and sleeps before joining a worker
(`clients/brood-client/src/remote.rs:184-230`). SSH and container shutdown call
blocking `Child::wait` with no timeout (`clients/brood-client/src/main.rs:
1214-1240`). Remote disconnect invokes these operations directly while
handling UI state (`clients/brood-client/src/main.rs:4079-4181`). During that
interval focus, actor output, resize deadlines, catalogues, and renderer events
accumulate. Even otherwise correct timeout and generation logic can then
observe artificial expiry or a burst of late results.

### File drop reads and decodes synchronously

The dropped-image path performs metadata lookup, whole-file read, format
detection, and preview decode on the presentation event path
(`clients/brood-client/src/main.rs:12256-12290`). Besides stalling input and
rendering, the metadata/read split is a file-identity time-of-check/time-of-use
window. The work should be scoped to a drop operation and target actor
generation, performed off-loop, and validated again at reducer admission.

## Risk order

The first remediation slice should cover paths that can paste wrong data,
apply the wrong user action, accept obsolete input, grow without bound, or
freeze the event loop:

1. clipboard selection epochs;
2. exact operation matching in session completion and keybinding persistence;
3. real release-build bounds for retained queues;
4. shared focus/input epochs across split lanes;
5. two-phase non-blocking backend teardown.

The second slice should establish revisioned snapshot and commit-receipt
primitives, then migrate session/catalogue/deployment flows. Picker and agent
intent lifetimes should migrate to the same operation primitive rather than
receiving new one-off booleans or string constants.

The third slice should move blocking platform services off the event loop and
add adversarial reducer, transport, actor, and nested-presentation tests. The
most useful failpoints are completion-before-publication,
publication-before-completion, cancel-before-create-reply, realm-change while
waiting, reversed layout completions, old clipboard reader last, saturated
critical input, pointer sample after leave, and disconnect during cleanup.

## Open design pressure

The main design choice is how much causal metadata belongs in shared protocol
envelopes versus subsystem payloads. A universal envelope is easy to validate
but risks meaningless fields and wire churn. Per-subsystem fields are compact
but have already led to missing identities at boundaries. A practical middle
ground is a small authenticated scope stamp in the envelope plus typed intent,
operation, revision, sequence, or epoch fields on messages that need them.

The other pressure is whether optimistic UI may advance before an
authoritative publication. Optimism is compatible with the target only when it
is stored separately from durable state, tied to one operation, and either
confirmed by a named commit revision or rolled back by the matching failure.
It must not overwrite the catalogue or pretend that completion order is source
order.

## Non-yielding Component dispatch can starve an independent worker task

During the pure-renderer publication authority probe on 2026-09-05, a worker
which was already active and yielding did not resume within ten seconds while
the same Component instance's synchronous actor dispatch spun. Ordinary dispatch
publication and standalone worker denial passed; the overlap diagnostic could
not establish worker progress and was removed from the passing probe rather
than represented as a successful overlap test.

Wasmtime 46 documents `StoreContextMut::async_call_stack()` as the current task's
youngest-to-root ancestry. The publication resolver checks for the current
actor dispatch task, so an independently started worker does not inherit that
authority. The failed overlap is liveness evidence, not an observed publication
permission leak.

This is a constraint on shared-instance actor/worker scheduling. The pure-renderer
target deliberately requires separately reachable facet execution while actor
work is busy; its separate-instance probes must continue to establish that
independence. This observation does not silently redefine actor-worker fairness
or count a suspended worker as a progress result.
