# WASM Execution Model

Manifold should run user code through two execution lanes with different contracts:

- a synchronous fast lane for bounded pre-commit policy
- an asynchronous lane for normal app, controller, extension, and agent behavior

The lanes should share the same workspace command and event model, but they should not share queues, budgets, or failure policy. Slow async work must not make latency-sensitive policy hooks contend with unrelated module activity.

## Current Runtime Boundary

The current `manifoldd` prototype is built around Smithay and calloop. The calloop event loop owns Wayland dispatch, input routing, timers, control socket handling, presentation attach/detach, and authoritative `Manifold` state.

That loop should remain the session process authority. Async plugin work can live in a separate runtime, likely Tokio-backed, connected to the calloop thread by bounded channels. Returned command requests must re-enter the calloop/session thread before mutating workspace state.

```text
calloop/session thread
  -> synchronous fast lane policy hooks
  -> commit workspace transaction
  -> append workspace events
  -> async plugin lane
  -> command requests back to calloop/session thread
```

## Fast Lane

The fast lane is for user-defined policy that must participate in a state transition before Manifold commits it. It exists for cases where asynchronous reaction would be too late or could create inconsistent ownership.

Examples:

- a newly mapped managed app surface needs classification, namespace ownership, or initial placement
- a key chord needs immediate command resolution
- a command needs preflight validation or rewrite
- a focus change needs allow, deny, redirect, or abstain policy

Fast lane hooks are synchronous and bounded. They must not perform slow or unbounded work.

Allowed:

- inspect a snapshot of the pending transaction
- return a claim, policy decision, command rewrite, placement request, or denial
- read small host-provided policy/config snapshots

Not allowed:

- mutate `Manifold` directly
- spawn processes
- perform filesystem, network, or blocking storage I/O
- wait on async work
- call arbitrary host APIs
- observe unrelated async plugin state unless the host snapshotted it into the hook context

Fast hooks should run with hard limits:

- strict CPU/time budget
- memory limit
- trap/timeout accounting
- deterministic ordering
- automatic disabling after repeated failures

The failure behavior should be defined per hook type. Most policy hooks should fail neutral, meaning Manifold ignores the failed hook and continues with host defaults. Security or validation hooks may fail closed if their manifest declares that behavior and the user/session grants it.

## Async Lane

The async lane is for ordinary user code. It should be event-driven and actor-like: each module owns its state, receives ordered events, and returns command requests instead of mutating Manifold directly.

Examples:

- update a WASM pane app after a pane event
- refresh a git/status UI
- run an agent workflow
- handle command registry actions
- perform storage or slow computation
- launch follow-up app actions through capability-checked commands

The async lane should support:

- bounded event queues
- backpressure diagnostics
- module lifecycle status
- hot reload
- per-module logs
- call latency and failure counters
- explicit command results

Async module code should not run on the calloop thread. A Tokio runtime can own this lane, but Manifold state remains authoritative on the calloop/session thread.

## Transactions And Events

Manifold should treat backend observations and user input as candidate workspace transactions.

```text
observe backend/input/control event
build pending transaction
run relevant fast hooks
resolve hook decisions
commit one workspace transaction
emit durable workspace events
deliver events to async modules
apply returned command requests through the command executor
```

This keeps Manifold as the only committer. Hooks and async modules can propose decisions or request commands, but they do not directly mutate compositor, presentation, or workspace state.

## Claim Resolution

Fast lane hooks should not take ownership directly. They should return claims that Manifold resolves once.

```rust
enum PolicyDecision {
    Abstain,
    ClaimNamespace {
        namespace: NamespaceId,
        priority: u32,
        reason: String,
    },
    Place(PresentationRequest),
    Deny {
        reason: String,
    },
}
```

For example, on a managed app map event:

```text
surface mapped
  -> PendingPane { surface, app_id, title, pid, ... }
  -> fast hooks return claim/place/deny decisions
  -> host resolves the winning namespace and placement request
  -> host commits PaneOpened with final owner
  -> losing hooks receive PaneClaimRejected or observe PaneOpened with final owner
```

This prevents two modules from both believing they moved the same pane into separate managed namespaces. The final committed event is the only source of truth.

Conflict policy should be explicit:

- highest priority wins when priorities differ
- equal priority uses stable module order or an explicit conflict error
- denied transactions report a reason in status/logs
- rejected claims are observable by the modules that made them

## Keyboard Shortcut Path

Shortcut matching should remain host-owned.

For simple bindings:

```text
key input
  -> host matches shortcut
  -> binding maps directly to WorkspaceCommand
  -> command executes immediately
```

For controller-backed bindings:

```text
key input
  -> host matches shortcut
  -> fast lane may resolve or rewrite the command
  -> host emits ShortcutInvoked
  -> async controller may request follow-up commands
```

The first implementation should keep shortcut bindings in normal mode only. That gives fast, predictable behavior without exposing raw keyboard observation. Raw keyboard observation should be a separate capability and should not be part of the first shortcut API.

## Runtime Shape

The first native implementation can avoid Wasmtime while proving the API:

- route existing normal-mode commands through a `WorkspaceCommand` executor
- add a native fast hook registry
- add a native async controller queue or mock executor
- expose hook and command counters through `status-json`

When Wasmtime is added:

- use one host-owned command/event model for both lanes
- keep fast lane instances separate from async lane instances
- use strict Wasmtime limits for fast hooks
- run async guest calls on the async runtime
- send all command requests back to the calloop/session thread

The fast lane should be an admission-controlled feature. A module should have to declare fast hooks in its manifest and receive explicit capabilities for them.

## Diagnostics

Both lanes need separate diagnostics.

Fast lane counters:

- registered hooks
- hook calls
- hook latency
- hook traps/timeouts
- disabled hooks
- claims made
- claims accepted
- claims rejected
- policy denials

Async lane counters:

- registered modules
- event queue length
- dropped or delayed events
- call latency
- command requests
- command denials
- traps/failures
- reloads

These should eventually appear in `status-json` so real workload tests can verify behavior without reading logs only.

## Design Rule

Use the fast lane only when Manifold needs a decision before committing workspace state. Use the async lane for everything else.

That preserves consistency for map, focus, command, and shortcut policy while keeping ordinary plugin work isolated from latency-sensitive paths.
