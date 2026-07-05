---
status: target
type: system
authority: product
last-reviewed: 2026-07-03
---

# Plugin Runtime

The target plugin runtime combines actor-style asynchronous execution with narrow policy providers for latency-critical host decisions.

The design rule is:

```text
async actors own plugin behavior and durable local state
policy providers answer immediate host queries from immutable published state
the host validates and commits all workspace, layout, input, and pane effects
```

## Runtime Shape

A plugin package may declare any number of actors and optional policy providers.

```text
Plugin
  Actor: background/index/status/command/pane state
  Actor: pane UI or service worker
  Policy provider: key policy, focus preflight
  Publications: default and named latest-state publications
```

The host provides:

- actor registry and addressing
- mailbox creation, routing, bounds, and diagnostics
- capability-checked native APIs
- policy provider registration and invocation
- named latest-state publication
- plugin lifecycle and failure handling
- command execution after validation

Plugins may be internally multi-threaded, but that is an implementation detail. The host-visible contract is actors, mailboxes, policy providers, and published immutable frames.

## Current Prototype

The first runtime slice is intentionally small: Manifold starts a built-in actor named `debug-toggle` on the Tokio runtime. The actor owns a bounded default mailbox slot backed by a Tokio `mpsc` channel and publishes an immutable boolean frame to its default publication. Each actor message toggles that boolean and appends a new host-owned frame.

A prototype key policy provider handles `Ctrl+O`. It reads the latest actor frame without waiting on the actor mailbox. If the frame says `spawn_btop_next=false`, the provider returns a host command to spawn `foot htop`; if it says `true`, the provider returns a host command to spawn `foot btop`. The provider also returns a mailbox message asking `debug-toggle` to toggle and publish the next frame.

Debugging entry points:

- DBus interface: `org.manifold.Plugin` at `/org/manifold/Plugin`
- CLI helper: `manifold ctl actors list`
- CLI helper: `manifold ctl actors mailboxes debug-toggle`
- CLI helper: `manifold ctl actors send debug-toggle <message>`
- CLI helper: `manifold ctl actors send-slot debug-toggle default <message>`

This prototype exercises actor mailboxes, host-owned publication append, synchronous key policy reads, returned host commands, and returned actor messages without making the actor a final plugin API.

## Mailboxes

Mailboxes are the normal communication path. They carry durable messages between plugin actors, host services, panes, agents, and other granted targets.

A mailbox is addressed by actor ID plus mailbox slot. The default slot is enough for simple actors; named slots let actors separate control, lifecycle, UI, and work traffic without making every message compete in one queue.

The native prototype backs each mailbox slot with a bounded Tokio `mpsc` channel. Host-side sends use `try_send`, including messages returned by policy providers, so the policy path never waits for actor work. Rejection increments diagnostics and is reported to the caller.

Mailbox delivery should support:

- bounded queues or explicit backpressure
- priority classes for lifecycle and shutdown
- sender/target/slot capability checks
- traceable message IDs and causality
- failure diagnostics for dropped, rejected, or delayed messages

A mailbox message may request a command, publish status, update pane UI, notify another actor, or report work completion. It should not assume immediate execution on the input or presentation thread.

## Policy Providers

Some host decisions cannot wait behind arbitrary actor queues. Keyboard routing, initial pane placement, focus preflight, and presentation layout often need a deterministic answer before the host can safely continue.

A policy provider exists for that bounded case. The host invokes a provider as a synchronous pure decision routine:

```text
HostQuery + immutable published state + host-provided query context
    -> PolicyResult { answer, directives, message_batch }
```

During the call, a policy provider may:

- read its presented state frame
- read host-provided query context
- compute a bounded result
- return a bounded message batch for later enqueueing

During the call, a policy provider must not:

- block on a mailbox or async task
- mutate host state directly
- perform filesystem, network, process, or storage I/O
- call arbitrary host APIs with side effects
- rely on processing a backlog of pending policy-path events

After the call returns, the host validates the result, applies accepted immediate decisions, and enqueues returned messages through normal mailbox rules.

## Publications

Publications are the bridge from async actors to policy providers.

```text
actor processes events asynchronously
actor asks host to append an immutable frame to a named publication
host atomically publishes the latest Arc-backed frame node
policy provider reads the latest frame during a host call
```

The default publication is enough for simple plugins. Named publications let a plugin split materialized policy state by worker, feature, or provider without inventing shared mutable memory.

Required guarantees:

- publishing is host-owned and atomic
- frames are immutable once visible
- frame count, size, and schema are bounded
- invalid frames are rejected before publication
- a policy invocation pins the `Arc` frame it reads for the duration of the call
- old frames are reclaimed only when no invocation can still read them
- frame payloads live in a host-owned namespace and do not contain plugin heap pointers

The policy provider should normally read the latest frame, not walk an unbounded history. The prototype stores frames as a bounded `Arc` linked list for pinning and diagnostics, but policy reads are latest-frame reads. If a policy needs history, the actor should materialize the needed bounded history into the latest frame.

Native in-process frames do not need to serialize through JSON. The safety rule is that plugins publish values into a host-owned frame namespace: owned data, offsets, handles, and resource IDs are valid; raw pointers or references into plugin heap/task memory are not valid published state. For WASM, host append should copy out of guest memory into this namespace before publication.

## WASM ABI Direction

The current prototype uses a narrow core-WASM path for key handling. That path proves the latency requirement but is not the final plugin API.

The target WASM ABI should preserve the same immediate-call property for pre-commit decisions while replacing direct side-effect imports with a returned decision envelope:

```text
key_policy(query, state_ref) -> KeyPolicyResult
```

Layout is actor/message based rather than a policy provider. Layout actors receive host events and send validated `SetResolvedLayout` host messages.

A `state_ref` for WASM should be a runtime-safe handle, copied buffer, or explicit read-only view. Do not require plugins to dereference raw host pointers unless the runtime model makes that safe and portable.

When the host needs to copy state into a guest policy provider, the guest should export a small arena allocator. The native scaffold expects `memory`, `manifold_arena_alloc(capacity, align) -> ptr`, and optionally `manifold_arena_free(ptr, capacity, align)`. The host calls the allocator, validates that the returned range is inside guest memory, then copies immutable frame/query bytes into that arena before invoking the policy export.

The actor scaffold also defines the first async loop shape: the guest exports `manifold_epoch_run(epoch, mailbox_ptr, mailbox_cap) -> i32` and imports `manifold.read_message() -> i32` plus `manifold.publish(slot_ptr, slot_len, payload_ptr, payload_len) -> i32`. The host allocates the mailbox arena up front, passes its pointer/capacity to each epoch call, and services `read_message` by awaiting the actor mailbox and copying the message bytes directly into that arena. The import returns the copied byte length, `-1` for a closed mailbox, or `-2` for host-side copy/allocation errors. When the guest calls `publish`, the host copies the named payload out of guest memory into a pending immutable publication frame. After the epoch returns, the host drains pending frames into the publication registry so policy providers can read the latest frame without touching actor state.

A single WASM module should self-declare its host-visible pieces through exports, so no sidecar manifest is needed for the first scaffold. `manifold_epoch_run` declares one default actor. `manifold_actor_<name>_epoch_run` declares a named actor. `manifold_policy_provider_key_<name>`, `manifold_policy_provider_map_<name>`, and `manifold_policy_provider_focus_<name>` declare policy providers. The `manifold-wasm` crate discovers these exports, registers actor mailboxes, instantiates one actor runtime per actor export, and records policy-provider handles for the next implementation step.

## Command And Event Flow

The host is the only committer:

```text
backend/input/control observation
  -> build candidate transaction or host query
  -> invoke relevant policy providers when needed
  -> validate decisions
  -> commit workspace/presentation state
  -> emit events
  -> deliver events to actor mailboxes
  -> execute accepted command requests on the host thread
```

Actors and policy providers may request commands or send messages. They do not directly mutate compositor state, presentation state, workspace state, pane focus, or native layout.

## Performance Policy

Mailbox passing is sufficient for normal plugin work. The policy provider and publication model exists for the narrow set of paths where queueing would create visible latency or inconsistent pre-commit ownership.

Performance-sensitive rules:

- use mailboxes for commands, events, and durable work
- use publications for latest policy state
- use policy providers only for host pre-commit decisions
- coalesce high-rate input before actor delivery when possible
- return handles or resource IDs for large payloads instead of copying through messages
- validate queue lengths and policy-provider latency in diagnostics

## Failure Policy

Every policy provider and actor should have observable failure state.

Policy provider failures should be neutral by default: disable or skip the failing provider, report the failure, and fall back to native policy. Security-style fail-closed hooks require an explicit manifest choice and grant.

Actor failures should not tear down the session process. The host should stop or restart the affected plugin components, preserve host-owned pane shells where possible, and render recovery UI for affected panes.

Related:

- [[../concepts/plugin]]
- [[input-shortcuts]]
- [[layout-policy]]
- [[plugin-ui]]
- [[extension-host]]
- [[../reference/wasm-fast-lanes]]
