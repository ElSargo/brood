---
status: target
type: system
authority: product
last-reviewed: 2026-08-01
---

# Plugin I/O And Resources

This page specifies the I/O, coordination, external-resource, and shutdown
contract for [[plugin-runtime]]. It describes the target design, not prototype
export names or a complete POSIX emulation.

UI and base canvas rendering belong to [[plugin-ui]] and [[plugin-graphics]].
Future GPU-style facilities are separated in [[plugin-advanced-graphics]]. Exact
current ABI details belong to [[extension-host]] and the code-linked
[[plugin-io-interaction-graph]].

## Invariants

1. An actor dispatcher handles one event and returns; it never waits on I/O or
   worker synchronization.
2. A worker wait suspends only that worker, never its actor or a Tokio executor
   thread.
3. Workers share their actor's memory and authority but have no public address
   or independent lifetime.
4. The host supplies operations, notifications, and pollables. Guest libraries
   own mutex, channel, semaphore, and queue layouts.
5. Every operation, handle, callback, and wait belongs to an actor generation
   or an explicitly broader resource scope.
6. All host queues, buffers, wait sets, and resource counts are bounded and
   observable.
7. Installed plugins are trusted session code. Resource APIs provide
   portability, lifecycle, and fault containment; authority comes from the
   enclosing local, containerized, or remote execution environment.

## Identity And Event Delivery

The host distinguishes a stable logical actor ID from each concrete actor
generation. Worker IDs are local to one generation; operation and resource IDs
include or imply their owning generation. Identifiers never silently wrap or
retarget a newer generation.

Actor event lanes are logically separate:

| Lane | Examples | Delivery policy |
| --- | --- | --- |
| Lifecycle | stop, resource lost, session shutdown | Reserved and highest priority |
| Completion | read, write, spawn, timer result | Reserved; one terminal result per operation |
| Worker | started, finished, failed, cancelled | Bounded actor-local events |
| Notification | shared state changed | Coalesced by generation |
| Mailbox | public semantic messages | Bounded with explicit rejection/backpressure |

The host selects among lanes. Guest code does not block one lane while waiting
for another.

## Actor Operations And Completions

An actor submits an operation and immediately receives an operation ID. The host
later delivers exactly one typed terminal completion:

```text
submitted -> completed(value) | cancelled | failed(error)
```

Cancellation races are resolved once. If cancellation wins, a successful late
result is discarded and cannot mutate guest memory. A completion is considered
delivered only after admission to the reserved completion lane.

Cancellation does not promise to roll back filesystem effects which began
before the request won. It does promise an ownership boundary: a blocking write
which has entered the host pool is joined before its `cancelled` completion is
delivered. Actor shutdown and hot replacement likewise join admitted blocking
writes before the old generation is considered stopped, so no filesystem
mutation from that generation can occur after cancellation is observable or a
replacement generation becomes active.

Large results use owned buffers, immutable blobs, or resource handles rather
than unbounded mailbox copies. Host pointers never enter guest memory, and the
host never writes through a buffer after returning its ownership.

SDK futures are state-machine sugar over submission and later completion. They
must return from actor dispatch rather than preserve a suspended actor stack.

## Worker I/O And WASI

Workers receive normal WASI resources where a stable interface exists. A
blocking-looking worker operation is implemented asynchronously:

```text
worker calls read
  -> host registers readiness and operation state
  -> only this worker suspends
  -> actor and sibling workers remain runnable
  -> worker resumes with the ordinary WASI result
```

Arguments, environment, working directory, and standard streams come from the
session, plugin configuration, and worker spawn request—not from accidental
inheritance of `broodd` internals. Stdin defaults closed unless supplied;
stdout and stderr default to bounded session/plugin logs unless attached to an
explicit stream.

Ordinary file and stream libraries should work. Brood does not promise
`fork`, arbitrary signals, devices, unrestricted `ioctl`, arbitrary `mmap`,
Unix account mutation, ambient descriptors, or system-wide process inspection.

## Shared Memory And Notifications

Workers communicate through guest-owned shared memory. A typical publication
uses a payload or queue plus an atomic generation counter:

1. Write the payload.
2. Increment the counter with release ordering.
3. Signal the host notification with the new generation.
4. Consumers load the counter with acquire ordering before reading.

The host records the greatest signalled generation. A notification means
“shared state may have changed,” not “one event occurred.” If every item
matters, items live in a guest queue and the notification tells consumers to
drain it.

Actor subscriptions are one-shot and lost-wakeup safe: registration atomically
compares the observed generation and either arms interest or queues an immediate
event. An actor handles the event with bounded `try_*` operations and re-arms
if needed. It never waits for a worker lock.

## Pollables And Worker Select

A pollable represents readiness or terminal state for a notification, timer,
stream, process, worker join, filesystem watch, socket, or cancellation token.
Workers use bounded `try_poll` and `wait_any`; SDKs build `select!`, deadlines,
and cancellation on top.

`wait_any` must:

- register the complete set without a lost wakeup;
- return immediately for an already-ready item;
- remove losing registrations before returning;
- report the selected item and readiness generation;
- support deterministic biased and rotating-fair selection;
- include worker cancellation;
- reject duplicate, stale, foreign, or over-limit handles.

Actors receive readiness as events instead of calling worker wait APIs.

## Resource Families

### Filesystem

Filesystem handles are rooted in directories made available by the session and
plugin configuration. Paths remain relative to those roots, and traversal or
symlink escape is rejected. This gives portable path semantics and prevents
accidental access; it is not presented as a hard boundary against a trusted
plugin which can launch native tools within the same session.

The useful baseline includes open, close, read, write, seek, flush, metadata,
directory iteration, create, rename, remove, and atomic replacement. Actors use
completion operations; workers may use ordinary WASI calls.

### Filesystem Watches

A logical watch is rooted in a filesystem handle and exposes a generation
notification plus bounded event batches. Events are advisory and may be
coalesced or duplicated by the platform. Every batch uses relative paths and
classifies create, modify, remove, rename, or generic change.

Both plugin backends use this model. Component guests own a typed WIT watch
resource and read typed batches; its SDK wrapper waits through the resource's
normal notification pollable. Preview 1 compatibility guests use the matching
handle operations and encoded batches. Neither backend owns a separate native
watcher or watcher-specific wait mechanism.

Event paths retain their native platform identity rather than passing through
lossy Unicode conversion. The Component record carries an opaque byte list:
raw `OsStr` bytes on Unix and little-endian UTF-16 code units on Windows.
Preview 1's line-oriented compatibility batch keeps printable ASCII readable
and encodes every other byte, including `%`, tab, and newline, as uppercase
`%HH`. This representation is reversible and cannot merge distinct filenames.

Overflow and any interval without complete coverage produce an explicit
`rescan` marker. Consumers maintaining state reconcile through the filesystem;
the event stream is not a journal. Backend-generic `changed` events are also
ambiguous for a stateful consumer: discard that entire batch and rebuild, just
as for `rescan`. A reconciliation scan is published atomically as the new model
before the consumer reports that reconciliation completed.

Recursive native registration runs as host-owned background work so a large
existing tree cannot monopolize actor dispatch. Registration
completion produces the first `rescan` marker because changes may race setup.

The host uses one daemon watcher service, deduplicates canonical native path
and recursion-mode registrations, and routes events into private bounded
subscription queues.
Logical handles retain independent actor-generation ownership, relative-path
projection, overflow, and teardown even when they share a kernel registration.
Generation teardown removes its subscriptions without disturbing other actors;
the final subscriber releases the native registration. Daemon status exposes
native registration, logical subscription, and in-progress setup counts so
operators can distinguish sharing from leaked resources.

### Streams And Blobs

Streams cover subprocess pipes, PTYs, sockets, host services, and incremental
tool output. Read and write ends expose bounded operations, EOF, typed failure,
half-close where meaningful, and readiness pollables. Full buffers apply
backpressure; capture is never silently unbounded.

Immutable blobs carry large cross-actor values without repeated mailbox copies.
They have fixed content and length, bounded host storage, explicit mapping or
copying, and generation-safe reference counting. Within one actor, shared-memory
snapshots are usually cheaper and carry a guest-defined revision so stale
worker results can be rejected.

### Clocks And Timers

Workers receive WASI clocks. Timers are pollables; actors arm timers and receive
events or completions. Scheduling and shutdown deadlines use monotonic time.
Timer count, horizon, and resolution are bounded.

### Processes

Subprocesses are a core plugin facility. The host provides direct spawn rather
than guest-controlled `fork` and `exec` replacement. A spawn request identifies:

- executable or tool;
- arguments, environment changes, and working directory;
- stdin, stdout, and stderr modes;
- actor or session lifetime;
- optional limits, deadline, and PTY settings.

The result contains a process handle, attached streams, and an exit pollable.
Operations include `try_wait`, wait, close stdin, terminate, kill, and bounded
status/resource queries.

A native child has the authority and controlled launch environment of its
session. This includes the session working directory, nested display, control
identity, surface-session identity, and isolated session bus. Supplying both
the display and bus matters for single-instance graphical applications: a
child must not contact an existing host-desktop instance and ask it to create
an unmanaged window. WASI directory handles do not sandbox that child. Brood
should state this plainly rather than add per-spawn approval prompts which
imply a boundary that does not exist. Containerized and remote sessions
constrain children at the session boundary.

Actor-generation processes terminate during actor shutdown by default.
Terminals and other durable services may explicitly create session-scoped
processes owned by the session service and reattach them to a replacement
actor. Detached unsupervised daemons are outside the model.

### PTYs

A PTY spawn adds one duplex bounded stream, a child lifecycle handle, validated
initial rows and columns, and an explicit terminal profile. The standard
`xterm-256color` profile deterministically supplies `TERM=xterm-256color` and
`COLORTERM=truecolor` and removes ambient color-policy variables such as
`NO_COLOR`, `CLICOLOR`, and `FORCE_COLOR`. This prevents the service launcher's
preferences from silently changing a terminal product. The child still starts
in the actor's resolved session working directory and receives host-controlled
session environment overrides. Executable selection remains capability-scoped
and allowlisted; guests cannot supply arbitrary environment entries per spawn.

Resize is a resource operation on the child handle. The host validates nonzero
dimensions against its PTY limit, retains only the native master control object
needed to apply the size, and releases it with the generation-owned child.
Component and Preview 1 guests have equivalent spawn-profile and resize
behavior wherever the compatibility contract exposes PTYs.

PTY output participates in worker `select!`. The public surface does not expose
arbitrary `ioctl`, termios state, descriptors, or unrestricted Unix signal
numbers.

The host waits for PTY child exit on the operating-system wait path rather than
polling child status. A separately owned termination handle wakes that wait
during explicit close or generation teardown, preserving immediate exit
notification and bounded shutdown without an idle timer.

### Networking

Workers should receive standard WASI socket interfaces when supported; actors
use completion operations over the same resources. DNS, proxy configuration,
address reachability, and credentials come from the session environment.
Connection counts and buffer use remain bounded for fault containment.

Unix-domain sockets need explicit session path semantics because they often
bridge authority outside a container. TLS may live in guest libraries over
ordinary streams or in a host/session service.

### Credentials And Host Bridges

Credentials are session authority. A local, containerized, or remote session
decides which environment variables, files, agents, sockets, or mediated secret
services exist. Plugins inside that session are trusted to use what is exposed.

Host bridges—such as host filesystem mounts, clipboard, devices, credential
forwarding, host process launch, and host session buses—must be explicit at the
session boundary. They should be observable and narrowly configured because
they determine what can cross that boundary, not because plugins are prompted
for every use.

### Supervision And Transactions

Plugin managers and session managers use host services rather than mutate
runtime implementation files. Actor supervision provides generation-aware
spawn, stop, replace, monitor, and diagnostics. Plugin installation uses an
atomic stage, validate, commit-or-rollback transaction. Session state,
checkpointing, actor membership, and durable resource reattachment use an
atomic session service.

These APIs protect host invariants and recoverability, not a security boundary
against the trusted caller. A worker may prepare downloads, indexes, or state,
but actor lifecycle and durable commits remain host-owned operations.

## Resource Scope And Shutdown

Every resource has one scope:

| Scope | Lifetime |
| --- | --- |
| Operation | Until one terminal completion |
| Worker | Until that worker exits |
| Actor generation | Default; until stop, crash, or replacement |
| Session | Owned by the session service and reattachable |
| Host | Internal service state, never directly guest-owned |

Moving a resource to a broader scope is explicit and host-managed. The normal
actor shutdown sequence is:

1. Stop admitting messages and new operations.
2. Cancel workers and remove wait registrations.
3. Cancel actor operations and close actor-scoped streams.
4. Terminate then kill actor-scoped processes within a deadline.
5. Join host work and resource supervisors.
6. Drop the guest address space and invalidate generation handles.
7. Publish the terminal actor event.

Session-scoped resources are detached before this sequence and remain owned by
the session service.

## Capabilities, Limits, And Diagnostics

Plugin manifests declare expected resources, host services, and budgets. The
session resolves their concrete roots, endpoints, tools, and limits before
activation. Workers inherit their actor's effective configuration and cannot
create authority outside the session environment.

These declarations are useful for configuration, audit, compatibility, and
future restricted deployments. In the normal trusted-plugin model they are not
a promise that hostile code is contained.

At minimum, diagnostics expose:

- mailbox depth and dispatch latency;
- live/runnable/waiting worker counts and time between yields;
- outstanding operations and completion depth;
- notification, pollable, and active wait counts;
- stream buffers, watch overflows, process state, and resource counts by scope;
- stream high-water marks and readiness polls that found no data or capacity;
- rejected, cancelled, stale, coalesced, and dropped work;
- causal actor generation, worker, operation, and message identity.

Backpressure is explicit. A full queue rejects admission or suspends a worker
through a pollable; it never silently loses an admitted mailbox message or
terminal completion.

Retired stream pressure remains visible for the actor generation after the
stream handle is dropped. This permits shutdown diagnostics to show both zero
live resources and the high-water/readiness behavior that preceded teardown.

## SDK Shape

The SDK exposes different actor and worker contexts. Actor types provide
submission, cancellation, worker spawn, notification subscription, bounded
shared-state access, messaging, and publication. Worker types provide WASI,
wait/select, streams, processes, deadlines, cancellation, and guest-owned
synchronization.

Actor-blocking APIs should be unavailable by type, not merely discouraged in
documentation.

## Verification

The contract is proven with real guests and observable runtime workloads, not only
mocked resource units. Required coverage includes:

- pending worker I/O while actor mailbox traffic progresses;
- notification and `select!` races without lost wakeups or leaked waits;
- ordinary worker filesystem and stream calls;
- process and PTY input/output, exit, cancellation, and backpressure;
- filesystem registration, change, overflow, and reconciliation;
- replacement under load with no stale completion or surviving actor resource;
- cancellation and replacement barriers around already-started blocking
  filesystem mutations;
- bounded stress and soak runs across many actor generations and workers.

The source-linked interaction map is [[plugin-io-interaction-graph]]. Current
behavior-test entry points are documented in [[testing]]. Open implementation
gaps live in the repository `issues.md` rather than in a historical phase plan
inside this target specification.

Related:

- [[plugin-runtime]]
- [[plugin-ui]]
- [[plugin-graphics]]
- [[plugin-advanced-graphics]]
- [[extension-host]]
- [[../concepts/plugin]]
- [[../concepts/session]]
- [[../decisions/0006-plugin-actors-and-workers]]
- [[../reference/wasm-fast-lanes]]
