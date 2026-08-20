# WASM Execution Model

This archived page records an earlier focused target for execution lanes and failure behavior. The current target has been promoted to [Plugin Runtime](../targets/systems/plugin-runtime.md), with related input and layout behavior in [Input And Keyboard Shortcuts](../targets/systems/input-shortcuts.md) and [Layout Actor](../targets/systems/layout-actor.md).

Use [WASM Integration](wasm-integration.md) only as historical context for the broader app and module model.

Brood should run user code through two execution lanes with different contracts:

- a synchronous policy lane for bounded pre-commit policy
- asynchronous actor/mailbox execution for normal app, controller, extension, and agent behavior

The lanes should share the same workspace command and event model, but they should not share queues, budgets, or failure policy. Slow async work must not make latency-sensitive policy hooks contend with unrelated module activity.

## Current Runtime Boundary

The current `brood` prototype is built around Smithay and calloop. The calloop event loop owns Wayland dispatch, input routing, timers, DBus control endpoint handling, presentation attach/detach, and authoritative `Brood` state.

That loop should remain the session process authority. Async plugin work can live in a separate runtime, likely Tokio-backed, connected to the calloop thread by bounded channels. Returned command requests must re-enter the calloop/session thread before mutating workspace state.

```text
calloop/session thread
  -> synchronous policy lane hooks
  -> commit workspace transaction
  -> append workspace events
  -> async actor/mailbox lane
  -> command requests back to calloop/session thread
```

## Policy Provider

The policy provider is for user-defined policy that must participate in a state transition before Brood commits it. It exists for cases where asynchronous reaction would be too late or could create inconsistent ownership.

Examples:

- a newly mapped managed app surface needs classification, namespace ownership, or initial placement
- a key chord needs immediate command resolution
- a command needs preflight validation or rewrite
- a focus change needs allow, deny, redirect, or abstain policy

Policy provider hooks are synchronous and bounded. They must not perform slow or unbounded work.

Allowed:

- inspect a snapshot of the pending transaction
- return a claim, policy decision, command rewrite, placement request, or denial
- read small host-provided policy/config snapshots

Not allowed:

- mutate `Brood` directly
- spawn processes
- perform filesystem, network, or blocking storage I/O
- wait on async work
- call arbitrary host APIs
- observe unrelated async plugin state unless the host snapshotted it into the hook context

Sync policy hooks should run with hard limits:

- strict CPU/time budget
- memory limit
- trap/timeout accounting
- deterministic ordering
- automatic disabling after repeated failures

The failure behavior should be defined per hook type. Most policy hooks should fail neutral, meaning Brood ignores the failed hook and continues with host defaults. Security or validation hooks may fail closed if their manifest declares that behavior and the user/session grants it.

## Async Actor Lane

The async actor lane is for ordinary user code. It should be event-driven and actor-like: each module owns its state, receives ordered events through mailboxes, and returns command requests instead of mutating Brood directly.

Examples:

- update a WASM pane app after a pane event
- refresh a git/status UI
- run an agent workflow
- handle command registry actions
- perform storage or slow computation
- launch follow-up app actions through capability-checked commands

The apolicy provider should support:

- bounded event queues
- backpressure diagnostics
- module lifecycle status
- hot reload
- per-module logs
- call latency and failure counters
- explicit command results

Async module code should not run on the calloop thread. A Tokio runtime can own this lane, but Brood state remains authoritative on the calloop/session thread.

## Published Sync-Path State

Async plugin work often needs to prepare state that a synchronous policy hook can use without replaying events or waiting on async code.

Brood should provide a host-owned publication area for this. The publication area is not shared mutable WASM memory and it is not a message queue. It is an immutable latest-state surface maintained by the host for a plugin instance.

The target shape is:

```text
async plugin worker
  -> processes workspace events
  -> publishes immutable payload to a slot
  -> host atomically publishes a new composite root

policy provider
  -> receives the current root reference
  -> reads the default slot or a named slot
  -> returns a bounded decision immediately
```

The public API should be the small general form:

```text
publish(name: option<string>, payload) -> generation
read(root, name: option<string>) -> option<published-record>
```

`none` names the plugin instance's default slot. `some(name)` names a plugin-scoped slot. Named slots are just a generalization of the default slot, so simple plugins can publish one materialized decision state while more complex plugins can let multiple async workers publish independent state.

For example, a navigation controller may publish one default state:

```text
publish(none, NavState {
  terminal-target,
  agent-target,
  editor-target,
  git-target,
})
```

Or split those into worker-owned slots:

```text
publish(some("terminal"), TerminalNavState { target })
publish(some("agent"), AgentNavState { target })
publish(some("editor"), EditorNavState { target })
publish(some("git"), GitNavState { target })
```

The host should internally retain enough old frames to keep sync-policy references valid for the duration of a call. The internal representation may use an Arc linked list, publication log, ring, or another RCU-like structure, but the policy provider should only see the current immutable root.

Host guarantees:

- publish is atomic
- published records are immutable
- sync-policy references remain valid for the hook call
- host metadata is sufficient to trace and debug frame publication without making generation numbers part of the plugin contract
- payload size, slot count, and reference depth are bounded
- malformed payloads are rejected before becoming visible
- trapping plugin code cannot corrupt host or plugin publication state

Published sync-path state should contain materialized decisions, not event history. Events remain the ordered stream of what happened. Published slots are the latest cache of what the plugin wants latency-sensitive hooks to see.

## Transactions And Events

Brood should treat backend observations and user input as candidate workspace transactions.

```text
observe backend/input/control event
build pending transaction
run relevant policy providers
resolve hook decisions
commit one workspace transaction
emit durable workspace events
deliver events to async actors
apply returned command requests through the command executor
```

This keeps Brood as the only committer. Hooks and async actors can propose decisions or request commands, but they do not directly mutate compositor, presentation, or workspace state.

## Claim Resolution

Sync policy hooks should not take ownership directly. They should return claims that Brood resolves once.

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
  -> policy providers return claim/place/deny decisions
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
  -> policy provider may resolve or rewrite the command
  -> host emits ShortcutInvoked
  -> async controller may request follow-up commands
```

The first implementation should keep shortcut bindings in normal mode only. That gives fast, predictable behavior without exposing raw keyboard observation. Raw keyboard observation should be a separate capability and should not be part of the first shortcut API.

## Runtime Shape

The first native implementation can avoid Wasmtime while proving the API:

- route existing normal-mode commands through a `WorkspaceCommand` executor
- add a native policy provider registry
- add a native async controller queue or mock executor
- expose hook and command counters through `status-json`

When Wasmtime is added:

- use one host-owned command/event model for both lanes
- keep policy provider instances separate from async actor lane instances
- use strict Wasmtime limits for policy providers
- run async guest calls on the async runtime
- send all command requests back to the calloop/session thread

The policy provider should be an admission-controlled feature. A module should have to declare sync hooks in its manifest and receive explicit capabilities for them.

## Diagnostics

Both lanes need separate diagnostics.

Policy provider counters:

- registered hooks
- hook calls
- hook latency
- hook traps/timeouts
- disabled hooks
- claims made
- claims accepted
- claims rejected
- policy denials

Apolicy provider counters:

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

Use a policy provider only when Brood needs a decision before committing workspace state. Use the async actor lane for everything else.

That preserves consistency for map, focus, command, and shortcut policy while keeping ordinary plugin work isolated from latency-sensitive paths.
