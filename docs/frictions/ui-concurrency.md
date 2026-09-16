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

### Clipboard selection readers can overwrite a newer selection

Each Wayland selection starts an independent untracked reader thread. The
thread unconditionally writes `latest_content` when it finishes
(`clients/brood-client/src/clipboard.rs:206-243,361-386`). A slow read for
selection A can therefore finish after selection B, or after `set_text`, and
replace the newer clipboard value. There is no selection epoch or offer
identity at commit time.

### Session command completions mutate unrelated newer state — remitted

`apply_completion` now requires `pending == Some(completion.request)` before
clearing pending or mutating error, status, selection, query, edit mode, or
activate/create realm identity—the same gate as directory-list and worktree
completions. Unmatched or late replies are no-ops and leave a still-pending
request in place for the real reply. Covered by unit tests in
`plugins/session-switcher/src/lib.rs`.

### Keybinding save replies survive cancellation

Closing the keybinding editor removes `pending_save`, but a later successful
reply still hides the editor, clears capture, changes layout visibility, and
opens the action picker because `handle_saved` does not require a matched save
before its success path (`plugins/keybindings/src/lib.rs:329-362,455-469`). The
reply names an action but no unique save attempt.

### Repeated backend-picker creation reuses one correlation value

Every discovery picker uses the constant request value `backend-discovery`.
Starting again clears the known picker identity, while any later reply with
that constant becomes the current picker
(`plugins/session-switcher/src/lib.rs:2464-2499,2908-2919`). A late reply from
an earlier open can replace or update the newer picker. More generally, picker
clients cannot cancel an in-flight creation before the service returns a
picker identity, so a late create reply can resurrect a closed modal.
The keybinding picker demonstrates this hole directly: close clears only the
boolean when no picker identity exists, while any later `picker-created` reply
is installed unconditionally (`plugins/keybindings/src/lib.rs:244-258,455-469`).

### Agent-picker intent changes realm while it waits

The pending agent picker is an unscoped boolean. A realm change clears agents
and subscribes to the new registry without clearing or rebinding that boolean
(`plugins/session-switcher/src/lib.rs:1092-1126,1940-1953`). The first registry
reply for the new session can fulfill a shortcut intent issued in the old
session and open an unexpected agent (`plugins/session-switcher/src/lib.rs:
2114-2137`).

### Layout completions can move durable state backward

The shell permits several pending layout requests. Any completed or degraded
request becomes `durable_shell_layout`, and its target revision becomes the
presented revision, without checking for a later request or higher revision
(`plugins/session-switcher/src/lib.rs:1843-1860,3593-3715`). If request 2
completes before request 1, the late request-1 completion can restore older
durable state and a lower presented revision.

## Authority and publication races

### Session and backend catalogues can regress each other

`SessionSnapshot.revision` is not checked when the switcher replaces all rows
for a backend (`plugins/session-switcher/src/lib.rs:1148-1182`). A separate
backend-catalogue path replaces the combined list and uses the absence of rows
for the current backend as its freshness heuristic
(`plugins/session-switcher/src/lib.rs:1184-1249`). The native client event that
installs a backend snapshot also has no connection generation or catalogue
revision and replaces routes, windows, sessions, and renderer state directly
(`clients/brood-client/src/main.rs:1113-1118,9453-9547`). Delayed non-empty
catalogue or snapshot events can therefore make a newer view older.

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

### Backend deployment results do not identify the initiating intent

SSH and container ready/failed client events carry only a deployment or error
string, and connection notifications sent to the switcher carry only backend
state and a message (`clients/brood-client/src/main.rs:1123-1132,9216-9284`;
`plugins/session-switcher/src/lib.rs:2057-2075`). A failure from an earlier
deployment can replace the status for a later attempt. The switcher cannot
settle the correct pending operation because the operation identity is lost
across the host boundary.

## Cross-lane ordering and pressure

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
