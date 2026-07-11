---
status: target
type: system
authority: product
last-reviewed: 2026-07-11
---

# Plugin Runtime

The target plugin runtime has three distinct execution roles:

```text
actors are public isolation, addressing, and lifecycle domains
workers are private shared-memory execution contexts inside an actor
policy providers answer immediate host queries from immutable published state
```

The host validates and commits all workspace, layout, input, pane, and process
effects. Neither actors nor workers receive direct authority over compositor or
renderer internals.

## Runtime Shape

A plugin package may declare any number of actors and optional policy providers.
Each actor may create many workers.

```text
Plugin
  Actor: address + mailbox slots + memory + capabilities
    Dispatcher: completion-driven, must not block
    Worker: actor-scoped task with shared memory
    Worker: actor-scoped task with shared memory
  Actor: independent address space and mailbox
    Worker: actor-scoped task with shared memory
  Policy provider: bounded synchronous decision routine
  Publications: host-owned immutable latest-state frames
```

Only actors and policy providers are registered with the host. Workers are
private implementation details of their owning actor and are never globally
addressable.

## Actors

An actor is the public unit of plugin isolation and communication. A WASM actor
owns a component instance or equivalent runtime container with its own:

- address space and durable local state
- globally addressable mailbox and named mailbox slots
- capability and resource table
- publication namespace
- lifecycle, diagnostics, and failure state

“Globally addressable” means addressable through the granted Manifold actor
registry. It does not imply ambient cross-session or cross-plugin access.

The actor is dormant while the host waits for its next event. Once invoked, the
actor dispatcher consumes one event, coordinates actor state, and returns
promptly. It must not suspend or block inside dispatch on filesystem, network,
process, timer, channel, lock, join, or other potentially unbounded operations.
It may:

- submit completion-based operations
- handle previously delivered completions
- spawn workers
- use non-blocking `try_*` synchronization operations
- send messages, publish state, and request host commands

This rule keeps actors responsive to input, lifecycle, cancellation, and host
control even while their workers are waiting on I/O.

## Workers

A worker is a lightweight execution context scoped to the actor that spawned
it. It acts like a thread inside the actor:

- it shares the actor's address space, local state, resources, and capabilities
- it may use ordinary blocking-looking WASI calls such as `read` and `write`
- it may use the same completion-based APIs as the actor
- it may use guest-owned shared-memory coordination with sibling workers
- it has a distinct stack or coroutine state and worker-local storage
- it has no actor ID, public mailbox, publication identity, or independent grant
- it cannot outlive its actor

A blocking-looking worker call suspends only that worker. The host runs the
underlying operation asynchronously and schedules other actor work while the
worker is waiting. Worker creation, join, cancellation, exit, and failure are
observable through actor-local handles and completion events rather than public
actor addressing.

Workers are the default tool for concurrent work that needs shared state or
ordinary library I/O. A separate actor is appropriate when work needs an
independent address, address space, capability set, failure boundary, or
lifecycle.

## Concurrency And Parallelism

The initial worker contract is cooperative concurrency. Many workers may have
I/O in flight, but only one execution context is required to execute guest code
at a time. A worker must yield at I/O, synchronization, explicit yield points,
or runtime scheduling boundaries so that it cannot monopolize its actor.

Cooperative workers do not promise multicore execution. Parallel shared-memory
workers would change synchronization and data-race semantics and must be
introduced as a separate explicit capability and API if a real workload
justifies them. The runtime must not silently turn cooperative workers into
parallel threads.

## I/O Model

Actors and workers use the same host I/O engine through two interfaces:

```text
actor submits operation
  -> host starts asynchronous work
  -> host posts a typed completion to the actor's completion lane
  -> actor handles the completion as a message

worker calls blocking-looking WASI operation
  -> host starts asynchronous work
  -> only that worker is suspended
  -> completion resumes that worker and returns the ordinary WASI result
```

The completion protocol is the canonical actor I/O contract. Language SDKs may
wrap it in futures or `async`/`await`, but those are ergonomic projections over
operation submission, suspension, completion, cancellation, and resumption.

Completion delivery uses a reserved actor-private mailbox lane so I/O results
do not compete with ordinary application messages. Each operation has a stable
actor-scoped ID and a typed terminal result. The runtime must bound outstanding
operations, preserve completion causality, support cancellation, and prevent a
completion from being delivered to a replacement actor generation after reload.

Large results should use owned buffers or capability handles rather than
unbounded message copies. A completion must not expose raw host pointers or
write into actor memory after ownership of the supplied buffer has returned to
the actor.

## WASI Compatibility

The plugin ABI should expose standard WASI interfaces with a Manifold-owned host
implementation. Manifold decides which files, streams, sockets, clocks,
processes, and other resources an actor receives; WASI supplies the portable
program-facing contract.

In worker context, normal language and libc operations such as `open`, `read`,
and `write` should work when the corresponding capability has been granted.
This is a WASI-supported POSIX-like compatibility promise, not a promise to
emulate every Linux syscall, signal, `ioctl`, `fork`, or ambient filesystem
behavior.

The preferred target is the WASI 0.3 Component Model concurrency and I/O shape.
Compatibility adapters may run WASIp1 or WASIp2 command-style code as a worker
inside an actor. Legacy blocking entry points must not become actor dispatchers.

Manifold-specific WIT interfaces should cover actors, mailbox slots,
publications, workers, capability-checked commands, and completion metadata.
They should extend standard WASI rather than replacing ordinary I/O with a
Manifold-only syscall vocabulary.

## Shared-Memory Coordination

The host does not implement mutexes, channels, condition variables, semaphores,
or ring-buffer layouts. Workers may build those structures in shared actor
memory using guest atomics plus host notification and pollable primitives.

Workers may wait on notifications or select across notifications, streams,
timers, process exit, joins, and cancellation. Waiting suspends only the worker.
The actor never waits on a worker synchronization primitive. Workers send
semantic actor-local events, or publish shared queue/snapshot changes by
incrementing an atomic generation and notifying the host. The host then delivers
a coalesced actor-private notification event.

SDKs should distinguish actor-owned state from memory explicitly shared with
workers. Actor dispatch uses acquire loads and bounded `try_*` operations after
a notification; mutex fairness and data-structure policy remain guest-owned.

The complete notification, worker-select, external-resource, and lifecycle
contract is specified in [[plugin-io-and-resources]].

## Capabilities, Limits, And Lifecycle

Workers inherit the owning actor's capabilities and may receive a narrower
derived set. Spawning a worker never grants new authority. Resource handles and
actor-local synchronization objects remain scoped to the actor address space.

The runtime must bound and report at least:

- live and peak worker count
- runnable, waiting, completed, cancelled, and failed workers
- outstanding I/O operations and completion backlog
- worker CPU/fuel use and time between yield points
- worker stack or coroutine-state memory
- actor mailbox depth and completion-lane depth
- notification and pollable counts, pending actor notification events, and
  active worker wait registrations

Stopping, crashing, or replacing an actor cancels its workers and outstanding
operations before releasing its address space and capabilities. A normal worker
failure terminates that worker and reports a typed completion to the actor.
Failures that invalidate shared runtime state may terminate the actor. Neither
case should tear down the session process.

## Mailboxes

Mailboxes are the normal durable communication path between actors, host
services, panes, agents, and other granted targets. Workers do not add another
host-visible addressing layer.

A mailbox is addressed by actor ID plus mailbox slot. The default slot is enough
for simple actors; named slots separate control, lifecycle, UI, work, and
completion traffic without making every message compete in one queue.

Mailbox delivery should support:

- bounded queues or explicit backpressure
- priority classes for lifecycle and shutdown
- sender, target, and slot capability checks
- traceable message IDs and causality
- failure diagnostics for dropped, rejected, or delayed messages

A mailbox message may request a command, publish status, update pane UI, notify
another actor, or report work completion. It must not assume immediate execution
on the input or presentation thread.

## Policy Providers

Some host decisions cannot wait behind arbitrary actor queues. Keyboard routing,
focus preflight, and other pre-commit decisions may need a deterministic answer
before the host can safely continue.

A policy provider is a synchronous, bounded, side-effect-free decision routine:

```text
HostQuery + immutable published state + host-provided query context
    -> PolicyResult { answer, directives, message_batch }
```

During the call, a policy provider may read its presented state and query
context, compute a bounded result, and return a bounded message batch. It must
not spawn or join workers, block on actor state, perform I/O, mutate host state,
or call arbitrary side-effecting APIs.

After the call returns, the host validates its result, applies accepted
immediate decisions, and enqueues returned messages through normal mailbox
rules.

## Publications

Publications bridge mutable actor state and synchronous policy providers.

```text
actor and workers update actor-owned state
actor asks host to append an immutable frame to a named publication
host atomically publishes the latest frame
policy provider reads the latest frame during a host call
```

Publications are actor-owned even when a worker prepared the data. Workers do
not receive independent publication identities.

Required guarantees:

- publishing is host-owned and atomic
- frames are immutable once visible
- frame count, size, and schema are bounded
- invalid frames are rejected before publication
- an invocation pins the frame it reads for the duration of the call
- frame payloads contain owned data, offsets, handles, or resource IDs rather
  than pointers into mutable actor memory

If a policy needs history, the actor should materialize the required bounded
history into its latest frame rather than making the provider drain a log.

## WASM Runtime Direction

The target WASM runtime should instantiate an independent Component Model actor
container for each actor declaration. Workers execute within that container and
share its guest state. The host event loop owns scheduling, asynchronous I/O,
completion delivery, cancellation, and resource limits.

Two runnable prototypes now exercise that execution model. `ComponentActor`
owns one component instance and Store, schedules cooperative async workers in
that container, and delivers typed I/O results through a reserved completion
lane. `Preview1CompatActor` is the POSIX-compatibility path: separate worker
Stores share an imported memory and standard Preview 1 authority, allowing an
ordinary blocking-looking worker `read` while the actor dispatcher continues.
Their actor completion APIs are backed by one bounded, generation-scoped
completion engine, and both runtimes apply structured worker/I/O shutdown
during replacement.

The runnable Component actor links only the Manifold WIT. Dispatch uses the
completion lane, and worker file/yield imports are admitted only from registered
worker guest tasks. Ordinary standard-library and libc I/O is currently provided
by the Preview 1 compatibility backend.

These implementations are evidence for the target, not a commitment to their
prototype export names. Package-manifest actor declarations and grants, the
complete registry/publication/policy surface, and an SDK that enforces the
dispatcher/worker type split remain target work. The Preview 1 path also has
Store-local descriptor tables even though its memory and authority are shared.
See [[extension-host]] for the runnable actor/worker proofs and
[[../reference/wasm-fast-lanes]] for the current core-WASM actor/policy lanes.

Policy providers remain isolated from mutable actor execution. They receive
host-owned immutable publication state rather than dereferencing worker or actor
memory.

## Command And Event Flow

The host remains the only committer:

```text
backend/input/control observation
  -> build candidate transaction or host query
  -> invoke a policy provider when an immediate decision is required
  -> validate and commit workspace/presentation state
  -> deliver durable events to actor mailboxes
  -> dispatch actor work or resume workers from completions
  -> validate and execute accepted command requests on the host thread
```

Actors and workers may request commands or send messages. They do not directly
mutate compositor state, presentation state, workspace state, pane focus, or
native layout.

## Performance And Failure Policy

Use actors for isolation and durable communication. Use workers for cheap
shared-state concurrency and blocking-looking compatibility I/O. Use policy
providers only for immediate host pre-commit decisions. Use publications for
latest immutable policy state.

Diagnostics must make actor latency, worker starvation, mailbox pressure,
completion pressure, policy-provider latency, cancellation, and failure visible.
The host should stop or restart affected plugin components, preserve host-owned
pane shells where possible, and render recovery UI without tearing down the
session process.

Related:

- [[plugin-io-and-resources]]
- [[../concepts/plugin]]
- [[../decisions/0006-plugin-actors-and-workers]]
- [[input-shortcuts]]
- [[layout-policy]]
- [[plugin-ui]]
- [[extension-host]]
- [[../reference/wasm-fast-lanes]]
- [WASI 0.3](https://bytecodealliance.org/articles/WASI-0.3)
- [Component Model concurrency](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Concurrency.md)
