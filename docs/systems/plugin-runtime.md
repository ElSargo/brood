---
status: target
type: system
authority: product
last-reviewed: 2026-07-03
---

# Plugin Runtime

The target plugin runtime combines actor-style asynchronous execution with narrow synchronous policy lanes for latency-critical host decisions.

The design rule is:

```text
async actors own plugin behavior and durable local state
sync policy lanes answer immediate host queries from immutable published state
the host validates and commits all workspace, layout, input, and pane effects
```

## Runtime Shape

A plugin package may declare any number of actors and optional sync policy lanes.

```text
Plugin
  Actor: background/index/status/command/pane state
  Actor: pane UI or service worker
  Sync policy lane: key policy, layout policy, map policy, focus preflight
  Swapchains: default and named latest-state publications
```

The host provides:

- actor registry and addressing
- mailbox creation, routing, bounds, and diagnostics
- capability-checked native APIs
- sync policy registration and invocation
- named swapchain publication
- plugin lifecycle and failure handling
- command execution after validation

Plugins may be internally multi-threaded, but that is an implementation detail. The host-visible contract is actors, mailboxes, sync lanes, and published immutable frames.

## Current Prototype

The first runtime slice is intentionally small: Manifold starts a built-in actor named `debug-toggle` on the Tokio runtime. The actor owns a bounded mailbox and publishes an immutable boolean frame to its default swapchain. Each actor message toggles that boolean and appends a new host-owned frame.

A prototype sync key lane handles `Ctrl+O`. It reads the latest actor frame without waiting on the actor mailbox. If the frame says `spawn_btop_next=false`, the lane returns a host command to spawn `foot htop`; if it says `true`, the lane returns a host command to spawn `foot btop`. The lane also returns a mailbox message asking `debug-toggle` to toggle and publish the next frame.

Debugging entry points:

- DBus interface: `org.manifold.Plugin` at `/org/manifold/Plugin`
- CLI helper: `manifold ctl actors list`
- CLI helper: `manifold ctl actors send debug-toggle <message>`

This prototype exercises actor mailboxes, host-owned swapchain append, synchronous key policy reads, returned host commands, and returned actor messages without making the actor a final plugin API.

## Mailboxes

Mailboxes are the normal communication path. They carry durable messages between plugin actors, host services, panes, agents, and other granted targets.

Mailbox delivery should support:

- bounded queues or explicit backpressure
- priority classes for lifecycle and shutdown
- sender/target capability checks
- traceable message IDs and causality
- failure diagnostics for dropped, rejected, or delayed messages

A mailbox message may request a command, publish status, update pane UI, notify another actor, or report work completion. It should not assume immediate execution on the input or presentation thread.

## Sync Policy Lanes

Some host decisions cannot wait behind arbitrary actor queues. Keyboard routing, initial pane placement, focus preflight, and presentation layout often need a deterministic answer before the host can safely continue.

A sync policy lane exists for that bounded case. The host invokes a lane as a synchronous pure decision routine:

```text
HostQuery + immutable published state + host-provided query context
    -> PolicyResult { answer, directives, message_batch }
```

During the call, a sync lane may:

- read its presented state frame
- read host-provided query context
- compute a bounded result
- return a bounded message batch for later enqueueing

During the call, a sync lane must not:

- block on a mailbox or async task
- mutate host state directly
- perform filesystem, network, process, or storage I/O
- call arbitrary host APIs with side effects
- rely on processing a backlog of pending sync-path events

After the call returns, the host validates the result, applies accepted immediate decisions, and enqueues returned messages through normal mailbox rules.

## Swapchains

Swapchains are the bridge from async actors to sync policy lanes.

```text
actor processes events asynchronously
actor asks host to append an immutable frame to a named swapchain
host atomically publishes the latest Arc-backed frame node
sync lane reads the latest frame during a host call
```

The default swapchain is enough for simple plugins. Named swapchains let a plugin split materialized policy state by worker, feature, or lane without inventing shared mutable memory.

Required guarantees:

- publishing is host-owned and atomic
- frames are immutable once visible
- frame count, size, and schema are bounded
- invalid frames are rejected before publication
- a sync invocation pins the `Arc` frame it reads for the duration of the call
- old frames are reclaimed only when no invocation can still read them
- frame payloads live in a host-owned namespace and do not contain plugin heap pointers

The sync lane should normally read the latest frame, not walk an unbounded history. The prototype stores frames as a bounded `Arc` linked list for pinning and diagnostics, but policy reads are latest-frame reads. If a policy needs history, the actor should materialize the needed bounded history into the latest frame.

Native in-process frames do not need to serialize through JSON. The safety rule is that plugins publish values into a host-owned frame namespace: owned data, offsets, handles, and resource IDs are valid; raw pointers or references into plugin heap/task memory are not valid published state. For WASM, host append should copy out of guest memory into this namespace before publication.

## WASM ABI Direction

The current prototype uses narrow core-WASM lanes for key handling and map hooks. Those lanes prove the latency requirement but are not the final plugin API.

The target WASM ABI should preserve the same immediate-call property while replacing direct side-effect imports with a returned decision envelope:

```text
key_policy(query, state_ref) -> KeyPolicyResult
layout_policy(query, state_ref) -> LayoutPolicyResult
```

A `state_ref` for WASM should be a runtime-safe handle, copied buffer, or explicit read-only view. Do not require plugins to dereference raw host pointers unless the runtime model makes that safe and portable.

## Command And Event Flow

The host is the only committer:

```text
backend/input/control observation
  -> build candidate transaction or host query
  -> invoke relevant sync policy lanes when needed
  -> validate decisions
  -> commit workspace/presentation state
  -> emit events
  -> deliver events to actor mailboxes
  -> execute accepted command requests on the host thread
```

Actors and sync lanes may request commands or send messages. They do not directly mutate compositor state, presentation state, workspace state, pane focus, or native layout.

## Performance Policy

Mailbox passing is sufficient for normal plugin work. The sync lane and swapchain model exists for the narrow set of paths where queueing would create visible latency or inconsistent input/layout ownership.

Performance-sensitive rules:

- use mailboxes for commands, events, and durable work
- use swapchains for latest policy state
- use sync lanes only for host pre-commit decisions
- coalesce high-rate input before actor delivery when possible
- return handles or resource IDs for large payloads instead of copying through messages
- validate queue lengths and sync-lane latency in diagnostics

## Failure Policy

Every sync lane and actor should have observable failure state.

Sync lane failures should be neutral by default: disable or skip the failing lane, report the failure, and fall back to native policy. Security-style fail-closed hooks require an explicit manifest choice and grant.

Actor failures should not tear down the session process. The host should stop or restart the affected plugin components, preserve host-owned pane shells where possible, and render recovery UI for affected panes.

Related:

- [[../concepts/plugin]]
- [[input-shortcuts]]
- [[layout-policy]]
- [[plugin-ui]]
- [[extension-host]]
- [[../reference/wasm-fast-lanes]]
