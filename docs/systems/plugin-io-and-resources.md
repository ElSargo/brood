---
status: target
type: system
authority: product
last-reviewed: 2026-07-11
---

# Plugin I/O, Coordination, And Resource Runtime

This document is the implementation specification for I/O, worker scheduling,
shared-memory coordination, external resources, and lifecycle in the target
plugin runtime. It refines [[plugin-runtime]] and the actor/worker decision in
[[../decisions/0006-plugin-actors-and-workers]].

The target is deliberately not a complete POSIX emulation. It provides the
portable operations needed by agents, terminals, editors, plugin managers, and
session managers while preserving Manifold's capability, actor, and lifecycle
boundaries.

Graphics, pane rendering, scene submission, and presentation integration are
outside this specification and remain covered by [[plugin-ui]] and later
graphics work.

Normative words such as **must**, **must not**, **should**, and **may** describe
the target contract rather than the current prototype.

## Core Contract

The runtime has four execution roles:

```text
actor dispatcher
  waits only by being dormant between host-delivered events
  never suspends or blocks after dispatch begins

worker
  shares its actor's address space
  may suspend on I/O, signals, timers, joins, and other pollables

policy provider
  runs synchronously against immutable publications
  never performs I/O or observes mutable actor memory

host
  owns scheduling, capabilities, external resources, lifecycle, and commits
```

The following invariants are mandatory:

1. An actor dispatcher runs one event to completion and returns promptly.
2. An idle actor is allowed to wait for its next event; no guest stack remains
   suspended while it does so.
3. A worker wait suspends only that worker, never the actor or a Tokio executor
   thread.
4. Workers share memory with their actor but do not receive a public mailbox,
   independent identity, or additional authority.
5. Worker-to-actor communication becomes an actor-local event. An actor never
   waits on a worker mutex, channel, condition variable, or join.
6. The host provides notification and polling primitives, not guest mutex or
   channel implementations.
7. External files, streams, processes, sockets, watches, timers, secrets, and
   durable resources are host-owned capability resources.
8. Every operation and resource belongs to an actor generation or to an
   explicitly broader host-managed scope.
9. A stale operation, message, notification, or handle must not affect a
   replacement actor generation.
10. All host-owned queues, operation sets, wait sets, buffers, resources, and
    shutdown paths are bounded and observable. Guest structures remain bounded
    by the actor memory limit.

## Runtime Shape

```text
Actor generation
├── one address space
├── actor dispatcher
├── public mailbox lanes
├── reserved lifecycle lane
├── reserved completion lane
├── coalesced notification lane
├── capability/resource table
├── host operation supervisor
└── workers
    ├── worker-local coroutine state
    ├── shared actor memory
    ├── worker-local storage
    └── poll/select registrations
```

The actor dispatcher is invoked by the host for one event. It is not a
long-running guest future. The host event loop performs the effective selection
among mailbox, lifecycle, completion, worker, and notification events.

Workers are long-lived guest tasks. A worker may use blocking-looking WASI or an
SDK future API. Both project onto the same host operation and pollable engine.

## Identity And Generations

Host-visible identities are generation aware:

```text
ActorLogicalId   stable identity used by sessions and registries
ActorGeneration  monotonically increasing concrete instance
ActorRef         logical ID plus generation
OperationId      actor generation plus sequence number
ResourceId       actor generation or explicit broader scope plus sequence
WorkerId         unique within one actor generation
```

Messages addressed to a logical actor may follow an explicit registry routing
policy. Messages, completions, and resource callbacks addressed to an
`ActorRef` must never be retargeted to a newer generation.

Sequence numbers must not be silently reused after overflow. Exhaustion stops
new allocation for that generation and reports a typed failure.

## Actor Event Model

An actor receives these event classes:

```text
LifecycleEvent     stop, capability revoked, resource lost, host shutdown
CompletionEvent    terminal result for an actor-submitted operation
WorkerEvent        worker started, message, finished, failed, cancelled
NotificationEvent  coalesced shared-memory publication signal
MailboxEvent       public or actor-local semantic message
WatchEvent         filesystem or other host resource changed
```

Lifecycle and completion traffic use reserved capacity. Notification events
are coalesced by notification identity. Ordinary mailbox traffic is bounded and
subject to backpressure.

The scheduler must prevent starvation across lanes. Lifecycle events may be
strictly prioritized, but completion or notification pressure must not
indefinitely suppress ordinary mailbox traffic. The exact weighted-fair policy
is runtime configuration, not guest ABI.

An actor dispatcher may:

- inspect and update actor-owned state;
- use bounded `try_*` operations over guest memory;
- submit host operations;
- spawn or cancel workers;
- send messages and publish immutable state;
- register one-shot notification interest;
- return host commands.

It must not:

- await a future;
- perform a blocking-looking WASI call;
- wait for a mutex, channel, signal, join, timer, stream, process, or socket;
- spin until shared state changes;
- retain a host callback or Store borrow after returning.

## Operations And Completions

An actor-facing host operation has this state machine:

```text
Submitted
  ├── Rejected synchronously
  ├── Running
  │   ├── Completed(result)
  │   ├── Failed(error)
  │   └── Cancelled
  └── Abandoned by actor shutdown
```

Submission returns immediately with either an `OperationId` or a typed
admission error. Admission errors include:

- capability denied;
- invalid argument;
- resource limit reached;
- command backlog full;
- actor stopping;
- resource stale or closed.

An admitted operation produces at most one actor-visible terminal completion.
Cancellation is a race: accepting a cancellation request does not promise that
cancellation beats a result already ready for delivery.

Completion delivery must:

- preserve the owning actor generation;
- retain result ownership until dispatch completes;
- use owned buffers or resource handles for large results;
- report truncation or configured limits explicitly;
- never write into guest memory after buffer ownership returned to the actor;
- release all host work before structured generation shutdown completes.

SDK futures for actors are allowed only as state-machine sugar that returns from
dispatch and resumes from a later completion. They must not suspend the actor
dispatcher stack.

## Worker I/O And WASI

Workers should receive standard WASI resources where a stable WASI contract
exists. Ordinary language operations such as file `open`, `read`, `write`, and
`poll` should work when granted.

The host implements blocking-looking worker operations asynchronously:

```text
guest worker calls read
  -> host registers operation and pollable
  -> guest worker coroutine suspends
  -> actor and sibling workers remain schedulable
  -> host readiness completes operation
  -> worker resumes with ordinary result
```

Manifold-specific WIT is appropriate for actor mailboxes, worker lifecycle,
notifications, host services, capability metadata, process management where
WASI has no adequate portable contract, and richer completion metadata.

The runtime does not promise all Linux/POSIX behavior. In particular, the
initial contract excludes `fork`, arbitrary signal delivery, ambient host
descriptors, devices, `ioctl`, arbitrary `mmap`, Unix user/group mutation, and
system-wide process inspection.

### Worker Arguments, Environment, And Standard Streams

A worker does not inherit the daemon's process context. Its WASI context is
constructed from the actor grant and spawn request:

- arguments come from package configuration or the worker spawn payload, not
  `manifoldd` arguments;
- environment contains only explicitly granted or actor-supplied keys;
- the logical current directory is a path inside a granted filesystem root;
- stdin is closed unless an explicit input stream is attached;
- stdout and stderr default to bounded actor-scoped log sinks, not daemon
  stdout/stderr;
- a spawn request may replace stdout/stderr with pipe, null, or another granted
  stream.

Worker stdin is not the fundamental actor/worker communication path. Plugins
normally pass initial shared-memory references or payloads when spawning and
then coordinate through guest structures plus notifications. An explicit WASI
stdin stream remains available for compatibility workloads that genuinely want
byte-stream input.

## Guest-Owned Shared-Memory Coordination

The host does not implement mutexes, channels, condition variables, semaphores,
or ring buffers. The plugin or SDK owns their memory layout, atomic protocol,
fairness, payload format, and allocation.

Plugins may build these structures from shared memory and atomics:

- bounded or dynamically sized channels within the actor memory limit;
- oneshots;
- work queues;
- mutexes and read/write locks used only by workers;
- condition variables;
- semaphores;
- worker pools;
- immutable snapshot publication slots.

Actor-owned application state remains logically single-owner even though it is
physically addressable by workers. SDK types should distinguish actor-owned
state from memory explicitly shared with workers. Workers should return
semantic results through actor-local events rather than require the actor to
take their locks.

## Notifications And Event Counters

The minimal host scheduling primitive is a notification resource paired with a
guest-owned atomic generation counter.

```text
guest shared memory
  AtomicU64 generation
  payload or queue

host resource table
  NotificationId
  latest announced generation
  actor subscription
  worker wait registrations
```

The SDK signal path is:

1. Write shared payload or queue entries.
2. Increment the guest generation with release ordering.
3. Call `notification_signal(notification, generation)`.
4. The host records the greatest announced generation, wakes workers, and
   schedules a coalesced actor notification if subscribed.

Consumers load the guest generation with acquire ordering before reading the
published data.

The host cannot observe arbitrary WebAssembly atomic writes. Calling the
notification host operation is mandatory. SDKs must combine the atomic
increment and host notification in one safe API.

### Generation Semantics

- Generation zero is reserved for initialization.
- Generations increase monotonically and must not wrap within a live resource.
- Concurrent signal calls may reach the host out of order; the host records the
  greatest generation.
- A notification communicates “shared state may have changed,” not one event
  per increment.
- If every item matters, items live in a guest queue and the notification only
  signals that the queue should be drained.

### Actor Subscription

Actors use a synchronous one-shot registration:

```text
watch(notification, observed_generation, token)
```

Registration atomically checks the latest announced generation. If it already
differs from the observed value, the runtime schedules the event immediately;
otherwise it registers interest. This prevents lost wakeups.

The delivered event contains notification identity, latest generation, and the
actor-provided token. After dispatch, the actor must re-arm if it still wants
interest. Multiple increments before dispatch coalesce into one event carrying
the latest generation.

An actor notification does not grant a mutex or reserve guest state. The actor
uses an acquire load and bounded `try_*` operation. Shared-state handoff and
fairness remain guest SDK policy.

### Worker Waiting

Workers receive:

```text
notification_try_wait(notification, observed_generation)
notification_wait(notification, observed_generation, deadline)
wait_any(pollables, deadline)
```

Wait registration and generation checking must be atomic from the guest's
perspective. A signal concurrent with registration either makes the wait
immediately ready or wakes the registered worker.

All waits are cancellation aware. Cancelling a worker removes every wait
registration before its terminal lifecycle event is delivered.

## Pollables And Worker Select

A pollable is a host resource representing readiness or terminal state:

```text
notification changed
timer expired
stream readable
stream writable
stream closed
process exited
worker terminated
filesystem watch changed
socket ready
cancellation requested
```

The host exposes bounded `try_poll` and asynchronous `wait_any`. The SDK builds
`select!`, futures, timeouts, and cancellation on top.

`wait_any` requirements:

- atomically register the complete wait set;
- return immediately for an already-ready item;
- remove losing registrations before returning;
- identify the ready item and readiness generation;
- define deterministic biased and fair modes;
- accept an optional monotonic deadline;
- always include implicit worker cancellation;
- reject duplicate, stale, foreign-actor, or over-limit pollables;
- document cancellation safety for operations whose future is dropped.

Waiting on a pollable is worker-only. Actor dispatchers receive readiness as
events instead.

WASI streams and sockets should project onto the same pollable engine so worker
SDK `select!` and ordinary WASI polling observe consistent readiness.

## Streams

Streams are capability resources with byte-oriented read or write ends. They
cover:

- subprocess stdin, stdout, and stderr;
- PTY masters;
- TCP and Unix-domain connections when granted;
- host service streams;
- captured logs;
- large incremental tool output.

Readable streams support:

- bounded read;
- optional exact read in worker SDKs;
- end-of-stream;
- typed failure;
- readable and closed pollables.

Writable streams support:

- bounded write;
- flush where meaningful;
- half-close;
- typed broken-pipe failure;
- writable and closed pollables.

Actors submit stream operations and receive completions. Workers may use async
SDK methods or blocking-looking WASI descriptors. A stream resource never
implies access to the daemon's native stdin, stdout, or stderr.

Buffers and queues are bounded. For a subprocess pipe, a full unread buffer
applies normal pipe backpressure unless the spawn policy explicitly selects a
bounded host spool or discard sink. Silent unbounded capture is forbidden.
A bounded log/discard sink may truncate only as an explicit configured policy;
it reports dropped byte counts and emits a visible truncation marker.

## Immutable Blobs And Shared Snapshots

Large values should not be copied repeatedly through actor mailboxes. The host
provides immutable blob resources for cross-actor transfer and durable
publications.

A blob:

- has a fixed byte length and content hash;
- is immutable after sealing;
- is bounded by capability and actor limits;
- may be referenced from messages and completions;
- may be mapped or copied into guest memory through an explicit operation;
- is reference counted by the host and generation safe.

Within one actor, plugins may instead use shared-memory snapshots. Snapshot
results must carry a guest-defined revision so the actor can reject stale
worker work, as required by editor parsing, search, and indexing.

## Filesystem

Filesystem authority is expressed as preopened directory capabilities or
equivalent resource handles. There is no ambient host root.

The initial worker filesystem contract includes:

- open and close;
- read, write, positional read/write, seek, and flush;
- metadata and directory iteration;
- create directory;
- create, truncate, rename, and remove when writable;
- links and symlinks only when separately permitted;
- atomic replace within one capability root;
- readable/writable polling where the underlying resource supports it.

Actor APIs expose completion equivalents for operations useful in dispatch.
SDKs may offer higher-level `read_file`, `write_file_atomic`, and transaction
helpers over the primitive operations.

Paths are relative to a granted root. Traversal outside that root is rejected.
Cross-root rename, device access, ambient descriptor inheritance, ownership
mutation, and Unix permission escalation are outside the default contract.

## Filesystem Watches

A filesystem watch is a host resource tied to a directory capability. It
provides a pollable for workers and coalesced events for actors.

Watch events include:

- path or relative path handle;
- created, modified, removed, or renamed classification;
- watch generation;
- overflow/rescan-required marker.

Events are advisory and may coalesce. Overflow must be explicit; consumers then
rescan through the granted filesystem capability. Watches must not reveal paths
outside their root through rename or symlink metadata.

## Clocks And Timers

Workers receive monotonic and wall clocks through WASI. Timers are host
pollables. Actors submit or arm timers and receive completion or timer events.

All deadlines use monotonic time. Wall-clock changes must not extend operation,
worker, process, or shutdown deadlines. Timer counts, maximum horizon, and
resolution are bounded.

## Processes

Subprocesses are required platform resources. They are not implemented as
`fork` followed by guest-controlled `exec`. The host provides direct spawn.

### Spawn Request

A spawn request contains:

```text
executable capability or declared tool ID
argument vector
sanitized environment additions
working-directory capability and relative path
stdin mode: pipe | null | supplied stream
stdout mode: pipe | null | bounded log sink
stderr mode: pipe | null | merge stdout | bounded log sink
process scope
resource limits and deadline
optional PTY configuration
```

No daemon file descriptor, environment variable, secret, or current directory
is inherited implicitly.

The result contains a process resource, optional stream resources, and a
process-exit pollable. Process operations include:

- `try_wait`;
- asynchronous `wait`;
- graceful terminate;
- forced kill;
- close stdin;
- query bounded status and resource usage.

Workers receive blocking-looking or async SDK wrappers. Actors submit spawn,
stream, wait, terminate, and kill operations and receive typed completions.

### Process Authority

An executable grant specifies either a package-declared tool or an allowed host
executable. A grant may constrain arguments, environment keys, working roots,
network access, child count, runtime, memory, and output.

Spawning a native process does not automatically preserve the WASM capability
sandbox. Until the host can project filesystem and network grants into an OS
sandbox, such a grant must be classified as privileged host execution and
shown explicitly to the user or administrator. The runtime must not claim that
a WASI preopen limits an unsandboxed native child.

### Process Ownership

The default process is actor-generation scoped. It is terminated during actor
shutdown or replacement.

Terminals require an explicit session-scoped process resource. A privileged
actor may request promotion to session scope. The host then owns the process,
issues a stable reattachment token, and allows a replacement actor with the
same session capability to reacquire it.

Detached arbitrary processes are not supported. Session scope is a supervised
host lifecycle, not Unix daemonization.

## PTYs And Terminal Processes

A PTY process is a specialized spawn mode providing:

- one PTY master stream;
- initial rows, columns, and terminal metadata;
- resize operation;
- foreground process group management by the host;
- terminal-safe interrupt, terminate, and kill operations;
- exit status and session-scoped reattachment when granted.

The initial API does not expose arbitrary `ioctl` or unrestricted Unix signal
numbers. It exposes the terminal operations required by shells and interactive
programs through typed host calls.

PTY output is a stream and therefore participates in worker `select!`. A
terminal plugin may place output in a guest shared-memory ring, increment a
notification, and let its actor drain and parse bounded batches.

## Networking

Network access is never inherited implicitly. A network grant specifies:

- allowed protocol: TCP, UDP, or explicitly enabled Unix-domain sockets;
- connect and/or listen authority;
- address, port, hostname, or Unix path restrictions;
- DNS permission;
- connection and bandwidth limits;
- optional proxy or session realm policy.

Workers should receive standard WASI socket interfaces when supported. Actors
use completion operations and readiness events over the same socket resources.

TLS may initially live in guest libraries over granted TCP streams. Access to
host certificate roots, client certificates, tokens, or proxy credentials is a
separate secret capability.

## Secrets And Approvals

Secrets are opaque host resources. Plugins request use by declared purpose and
service identity. Secret bytes should not enter guest memory unless a grant
explicitly permits export. Prefer host-mediated credential application to
network or process requests.

Potentially dangerous process, filesystem, network, plugin-management, and
session-management operations may require approval. Approval is asynchronous:
the actor receives a completion or lifecycle event after the host or a granted
approval actor decides. An actor dispatcher never blocks waiting for UI.

Approval records include actor identity, generation, requested capability,
human-readable effect, decision, and expiry. Coding agents additionally record
approved and performed actions in the session-local agent action log.

## Actor Messaging And Worker Results

Workers communicate with their actor through actor-local messages or shared
memory plus notifications. Actor-local messages are bounded semantic events;
notifications are appropriate for high-frequency shared queues or snapshots.

Workers do not acquire a public actor address. The runtime associates every
worker result with its owning actor generation and worker ID.

Actor-to-actor messages support:

- sender and recipient `ActorRef`;
- named mailbox slot;
- trace/message ID and correlation ID;
- inline bounded payload or immutable blob handle;
- explicit enqueue result and backpressure;
- optional reply endpoint with expiry;
- capability check for target and slot.

Pub/sub and frontend attachment should be implemented as host registry or
stream services rather than unbounded duplication into one actor mailbox.

## Actor Supervision

Supervision is a host capability used by session managers, plugin managers, and
other trusted system actors. It includes:

- spawn declared actor;
- stop actor;
- monitor actor generation;
- request restart or replacement;
- receive lifecycle events;
- query bounded diagnostics;
- delegate a narrower capability set.

Ordinary plugin actors do not automatically receive authority over other
actors. A worker cannot spawn a public actor unless its owner has delegated an
explicit request path and the actor dispatcher commits it.

The supervisor distinguishes logical identity from generation. A monitor event
always identifies the concrete generation which changed.

## Privileged Host Services

Some use cases need transactional host services rather than raw I/O:

### Plugin Management

Plugin installation uses a host-owned transaction:

```text
begin -> stage -> verify -> validate manifest -> commit | rollback
```

Download, hashing, signature verification, and compilation may run in workers.
Only the host transaction activates, replaces, disables, or removes installed
plugins. Updating the plugin-manager actor itself must not invalidate an
in-progress host transaction.

### Session Management

Session mutation, durable checkpointing, actor membership, process promotion,
and restoration use a privileged session service. Durable state commits are
atomic and generation aware. A normal plugin cannot obtain session control by
opening implementation files directly.

## Resource Scope And Lifecycle

Every resource declares one scope:

```text
operation scope       exists until one operation completes
worker scope          closes when the worker terminates
actor-generation scope default; closes on stop, crash, or replacement
session scope         host supervised; may be reattached by authorized actors
host scope            internal service resource, never directly owned by guest
```

Promotion to a broader scope is an explicit privileged operation. Dropping the
last guest handle does not necessarily end a session resource; the session
service owns its lifecycle.

Shutdown order for an actor generation is:

1. Stop admitting public mailbox messages and new operations.
2. Deliver or assert cancellation to workers.
3. Remove worker poll and notification registrations.
4. Cancel actor-generation operations.
5. Close actor-generation streams and process stdin.
6. Gracefully terminate actor-generation processes.
7. Kill processes that exceed the shutdown deadline.
8. Join blocking host work and resource supervisors.
9. Drop the guest Store/address space.
10. Emit the terminal actor-generation event.

Session-scoped resources are detached from the generation before this sequence
and remain owned by the session service.

## Capability Manifest

A plugin package declares required and optional capabilities. Grants are
resolved before actor activation and may be narrower than requested.

Capability families include:

- filesystem roots and operations;
- filesystem watches;
- network endpoints and DNS;
- declared executables and process sandbox profiles;
- PTY and session-process promotion;
- environment keys and working directories;
- secrets and credential application;
- actor registry targets and mailbox slots;
- actor supervision;
- plugin-management transactions;
- session-management operations;
- resource, buffer, worker, and operation limits.

Workers inherit their actor's effective grants and may receive a narrower
derived view. Spawning a worker never grants new authority.

Revocation produces a lifecycle event, cancels affected operations, and makes
future use fail with a typed revoked-capability error.

## Limits, Backpressure, And Diagnostics

The host must configure and expose at least:

- actor mailbox capacity by lane;
- notification count and pending actor notification count;
- worker count and wait-set size;
- outstanding operations and completion depth;
- stream buffer and per-operation transfer sizes;
- immutable blob count and bytes;
- file watch count and overflow count;
- process count, output bytes, runtime, and termination status;
- socket count and bandwidth;
- CPU fuel, wall deadline, and time between worker yield points;
- resource count by type and scope;
- rejected, cancelled, stale, coalesced, and dropped event counts.

Backpressure is explicit. A full queue returns a typed error or suspends a
worker through a pollable; it never silently discards an admitted operation or
mailbox message. Coalescing is permitted only for documented level-triggered
state such as notification generations and filesystem rescan markers.

Diagnostics identify actor logical ID, generation, worker or operation ID,
resource scope, capability, and causal message ID without exposing secret
payloads.

## SDK Contract

The SDK exposes distinct actor and worker contexts.

Actor context includes:

```text
try_send_message
submit_operation
cancel_operation
spawn_worker
cancel_worker
watch_notification
try shared-memory operation
publish immutable state
request host command
```

Worker context includes:

```text
blocking-looking WASI resources
async stream/process/socket operations
wait and wait_any
select!
sleep and deadlines
cancellation token
actor-local send
guest channels, locks, semaphores, and joins
```

Guest synchronization types are SDK libraries over shared atomics and host
notifications. They are not host resources except for their notification and
pollable handles.

The Rust SDK should make actor-blocking operations unavailable by type rather
than merely documenting that they are forbidden.

## Illustrative ABI Shape

Exact package names and generated bindings remain implementation details, but
the durable concepts should resemble:

```wit
interface notifications {
    resource notification;

    create: func() -> result<notification, notification-error>;
    signal: func(
        notification: borrow<notification>,
        published-generation: u64,
    ) -> result<u64, notification-error>;
    watch-actor: func(
        notification: borrow<notification>,
        observed-generation: u64,
        token: u64,
    ) -> result<watch-result, notification-error>;
    unwatch-actor: func(notification: borrow<notification>);
}

interface worker-poll {
    resource pollable;

    notification-pollable: func(
        notification: borrow<notification>,
        observed-generation: u64,
    ) -> result<pollable, poll-error>;
    try-poll: func(pollables: list<borrow<pollable>>) -> list<u32>;
    wait-any: async func(
        pollables: list<borrow<pollable>>,
        deadline-ns: option<u64>,
        mode: wait-mode,
    ) -> result<wait-ready, poll-error>;
}

interface processes {
    resource process;
    resource input-stream;
    resource output-stream;

    spawn: async func(spawn-request) -> result<spawned-process, process-error>;
    try-wait: func(process: borrow<process>) -> option<exit-status>;
    wait: async func(process: borrow<process>) -> result<exit-status, process-error>;
    terminate: async func(process: borrow<process>) -> result<_, process-error>;
    kill: async func(process: borrow<process>) -> result<_, process-error>;
}
```

Actor worlds expose submission, cancellation, watch registration, and event
types. Worker worlds expose asynchronous functions and standard WASI resources.
Worker-only waiting imports must reject invocation from actor dispatch.

## Implementation Sequence

### Phase 1: Coordination Foundation

- Add notification resources and monotonic generations.
- Add coalesced actor notification events and one-shot watch registration.
- Add worker notification wait and bounded wait-any.
- Add cancellation-safe poll registration teardown.
- Build SDK `Signal`, bounded channel, oneshot, cancellation token, and `select!`.
- Replace synthetic actor-to-worker stdin as the primary communication model;
  retain it only as an optional WASI stream adapter if useful.

### Phase 2: Generic Streams And Timers

- Introduce input/output stream resources and pollables.
- Project stream operations into actor completions and worker waits.
- Add monotonic timers and deadlines.
- Add bounded blob resources for large results.
- Prove fair selection across notification, timer, and stream readiness.

### Phase 3: Processes

- Add direct spawn with pipe/null/log modes.
- Add wait, terminate, kill, exit pollable, and structured shutdown.
- Add executable grants, sanitized environment, working directory, and explicit
  privileged-host-execution classification where OS sandboxing is absent.
- Add output backpressure and process diagnostics.

### Phase 4: PTY And Session Resources

- Add PTY spawn, resize, interrupt, and terminal process control.
- Add session-scoped process promotion and authorized reattachment.
- Verify terminal actor replacement without terminating its shell.

### Phase 5: Filesystem Watches And Networking

- Add capability-rooted watches and overflow/rescan semantics.
- Add capability-filtered TCP/UDP/DNS and pollable readiness.
- Add Unix-domain sockets only after path capability semantics are defined.

### Phase 6: Supervision And Privileged Services

- Add generation-safe actor monitoring and lifecycle commands.
- Add plugin installation transactions.
- Add durable session state and resource reattachment.
- Add approval and secret-resource integration.

## Verification Requirements

The implementation is not complete until real workloads prove these cases:

### Coordination

- Multiple workers use an SDK shared-memory channel under contention.
- A worker waits without spinning and sibling workers plus the actor progress.
- `select!` races two notifications, a timer, and cancellation without lost
  wakeups or leaked registrations.
- An actor receives a coalesced notification, performs an acquire load, drains
  shared work with bounded `try_*` operations, and returns.

### Coding Agent

- One agent actor runs concurrent network and tool workers.
- A subprocess streams stdout and accepts stdin while cancel and approval
  messages remain responsive.
- Multiple frontend actors attach, detach, and catch up without flooding the
  agent control mailbox.

### Terminal

- A PTY worker selects over PTY output, actor input, resize, exit, and
  cancellation.
- Output backpressure remains bounded.
- Replacing the terminal actor reattaches to a session-scoped shell without
  losing the process or accepting stale events from the old generation.

### Plugin Manager

- A worker downloads and verifies an artifact.
- The host commits or rolls back installation atomically.
- Updating the manager actor does not corrupt an in-progress transaction.

### Session Manager

- A privileged actor monitors actor generations and supervised resources.
- A crash during durable state update recovers the previous or new complete
  transaction, never a partial state.

### Editor

- Document actors remain responsive while workers parse, search, and index
  immutable revision-tagged snapshots.
- Stale worker results are rejected.
- LSP streams, file watches, debounce timers, and cancellation share one worker
  select loop.

### Shutdown And Pressure

- Actor replacement under full mailboxes, completions, notifications, streams,
  and process output terminates deterministically.
- No worker, host blocking task, process, wait registration, or actor-generation
  resource survives shutdown unless explicitly session scoped.
- Every rejected or coalesced action is observable in diagnostics.

## Migration From The Current Prototype

The coordination foundation is implemented in both runtime backends. It
provides generation-scoped notifications, one-shot coalesced actor watches,
notification pollables, bounded worker `wait-any`, biased and rotating-fair
selection, relative deadlines, cancellation wakeup, and observable wait
registration teardown. The Preview 1 workload now uses a guest-owned atomic
queue plus notifications as its primary actor/worker path. Its host-owned stdin
stream remains only as an explicit ordinary-WASI compatibility adapter.

The Component workload proves the same notification contract alongside actor
completion I/O, task-scoped worker imports, cooperative cancellation, and
dispatcher progress. The Preview 1 workload additionally proves ordinary
worker stdin `read`, filesystem `open`/`read`/`write` through Rust `std::fs`,
shared linear memory, and generation-safe replacement while workers are
suspended in notification wait sets.

Generic streams and timers, processes, PTYs, networking, file watches, blobs,
supervision, and privileged transactions remain later phases. Notification
pollables are therefore the only currently implemented pollable kind.

## Explicit Non-Goals

The initial runtime does not attempt to provide:

- `fork` or guest-controlled `exec` replacement;
- arbitrary Unix signals, process groups, or job-control syscalls outside the
  typed terminal/process API;
- unrestricted daemon stdio inheritance;
- arbitrary device files or `ioctl`;
- arbitrary file-backed `mmap`;
- Unix user/group database or ownership mutation;
- system-wide process inspection;
- advisory file locking until a concrete cross-process workload requires it;
- guest dynamic linking against arbitrary host libraries;
- ambient network or filesystem access;
- detached unsupervised child daemons;
- host-implemented mutex, channel, or condition-variable layouts;
- multicore shared-memory workers without a later explicit parallelism design.

Related:

- [[plugin-runtime]]
- [[extension-host]]
- [[../concepts/plugin]]
- [[../decisions/0006-plugin-actors-and-workers]]
- [[../reference/wasm-fast-lanes]]
