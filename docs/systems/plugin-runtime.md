---
status: target
type: system
authority: product
last-reviewed: 2026-07-21
---

# Plugin Runtime

Manifold treats an installed and approved plugin as trusted user code in its
execution environment. Actors may run in the session backend or in a
presentation client according to explicit package placement. The runtime
provides fault isolation, scheduling, lifecycle, and a portable I/O environment;
it does not claim that a useful coding plugin is an adversarial security sandbox.
A remote backend cannot authorize code execution on a client merely by sending
it a WASM artifact; see [[presentation-client]].

The complete model has three execution roles:

| Role | Purpose | May wait on I/O? | Isolation |
| --- | --- | --- | --- |
| Actor | Public identity, mailbox, state, and lifecycle | Only while dormant between events | Own address space |
| Worker | Private concurrent work inside an actor | Yes; only that worker suspends | Shares its actor's memory |
| Policy provider | Immediate bounded host decision | No | Reads immutable published state |

The detailed I/O and resource protocol is specified in
[[plugin-io-and-resources]].

## Trust And Isolation

Installing and enabling a plugin is equivalent to choosing to execute it as the
current session user. A useful coding plugin may read and modify the workspace,
run tools, use the network, and interact with session services. Repeated
per-operation permission prompts do not turn that authority into a dependable
security boundary and are not part of the normal runtime model.

Isolation still matters:

- each actor has its own address space and failure state;
- workers share memory only with their owning actor;
- queues, operations, handles, and resources are bounded and generation scoped;
- a plugin crash or runaway worker must not tear down the session;
- the host validates ABI data and owns compositor, renderer, and lifecycle
  invariants.

These are fault-containment and correctness guarantees. They protect the system
from bugs, stale work, and resource exhaustion, not from a malicious installed
plugin with legitimate session authority.

Authority is bounded only by the enclosing session environment. A direct local
session has the full authority of its user. Container and remote sessions add a
clear environment boundary which defines the available filesystem mounts,
processes, devices, network, credentials, and host bridges. Plugins within any
one environment are trusted peers. See [[../concepts/session]].

## Runtime Shape

```text
Session backend                         Presentation client
├── session actor                       ├── presentation actor
│   ├── worker                          │   ├── worker
│   └── worker                          │   └── worker
├── backend policy/service lanes        ├── local key decision + layout actor
└── immutable publications              └── local UI and graphics publications
                 bounded addressed messages
```

Only actors and policy providers are registered with their local host. Workers
are private implementation details and are never globally addressable. Same-host
messages remain local; explicitly addressed cross-placement messages are routed
over the presentation connection. Actors do not transparently migrate between
placements.

Capabilities describe the resources and host APIs a plugin expects inside its
session. They support configuration, audit, portability, and least-accidental
authority, but they are not advertised as protection from trusted plugin code
or from a native subprocess launched by that plugin.

## Actors

An actor is the public unit of plugin state, communication, and fault isolation.
It owns:

- an address space and placement-local state;
- a globally addressable mailbox with optional named slots;
- a resource table and publications;
- workers, outstanding operations, diagnostics, and lifecycle state.

An actor is dormant while waiting for its next host-delivered event. Once
dispatch begins it handles one event and returns promptly. Dispatch must not
wait on I/O, a worker lock, a channel, a join, or another unbounded operation.
It may submit completion operations, spawn workers, use bounded `try_*`
operations, send messages, publish state, and request host actions.

This rule is about scheduler responsiveness, not security. It keeps lifecycle,
input, cancellation, and mailbox traffic moving while background work waits.

Use another actor when work needs an independent address, memory space, failure
domain, or lifecycle.

## Workers

A worker acts like a thread inside one actor:

- it shares the actor's memory, state, resources, and effective grants;
- it has private stack or coroutine state and worker-local storage;
- it may use ordinary blocking-looking WASI calls such as `read` and `write`;
- it may select across streams, timers, processes, notifications, and
  cancellation;
- it has no public mailbox, publication identity, or independent lifecycle;
- it cannot outlive its actor.

Blocking-looking calls suspend only the calling worker. The host performs the
operation asynchronously and remains free to run the actor and sibling workers.

Workers coordinate through guest-owned shared-memory structures, atomics, and
host scheduling notifications. The host does not impose mutex, channel,
condition-variable, or ring-buffer layouts. An actor observes shared results
through bounded `try_*` operations or semantic actor-local events; it never
waits for a worker to release a mutex.

The baseline contract is cooperative concurrency. Multicore shared-memory
execution may be added explicitly later, but the runtime must not silently
change the data-race semantics of existing workers.

## I/O

Actors and workers project onto one host I/O engine:

```text
actor submit -> operation ID -> typed completion event
worker call  -> suspend worker -> ordinary WASI result
```

Actor completion lanes are reserved from ordinary application mailboxes.
Completions, resources, and cancellation belong to one actor generation, so a
late result cannot enter a replacement actor.

Standard WASI is preferred for files, streams, sockets, clocks, and other
portable facilities. Manifold APIs add actor addressing, workers,
notifications, publications, and host services rather than replacing normal
language I/O with a bespoke syscall vocabulary. Compatibility does not imply a
complete Linux emulation: `fork`, arbitrary `ioctl`, ambient descriptors, and
system-wide process control are outside the contract.

### Bounded streams, processes, PTYs, sockets, and timers

The portable host surface uses one bounded byte-stream resource for child
standard input/output/error, PTY master traffic, and connected sockets. Reads
are nonblocking and distinguish data, temporary unavailability, and EOF;
writes either admit a bounded chunk or report backpressure. Closing the write
side is explicit. There are no process-, terminal-, socket-, or timer-specific
wait calls.

Every host-owned resource exposes a scheduling notification. A stream advances
its notification generation when bytes become readable, write capacity
changes, EOF arrives, or an error becomes terminal. Child exit and timer expiry
advance their own notifications. Workers turn those notifications into normal
pollables and select with the same bounded `worker-wait-any` operation used for
guest notifications and filesystem watches. After readiness, the guest reads
typed stream, child-status, or timer-status state; notifications never carry
the data themselves.

Process authority is an explicit manifest allowlist of PATH-resolved command
names (`subprocess:<name>`). The runtime reports the resolved executable used
for a child so observable proofs and diagnostics do not hide environment
selection. Pipe and PTY children start from the same actor/session working
directory; terminal setup must pass that directory explicitly rather than
infer it from `HOME`. `pty` separately permits spawning a child attached to a
real host pseudoterminal. `network:loopback` permits outbound connections only
to a loopback peer; accepting listeners, DNS, ambient sockets, and arbitrary
remote addresses are not part of this surface. PTY children still require
their command allowlist entry.

Children, streams, PTYs, connected sockets, timers, pump tasks, and their
notifications belong to one actor generation. Generation stop first rejects
new operations and invalidates all handles and derived pollables, then closes
streams and sockets, kills live children, cancels timers and pumps, and joins
all host work before replacement can be published. Count, buffered-byte, and
write-chunk ceilings apply uniformly to Component and Preview 1 guests.

## Messaging, Policy, And Publications

Mailboxes are the durable communication path between actors, host services,
panes, and agents. Mailboxes are bounded, reject or expose backpressure, and
carry sender, target, slot, generation, and causal identity. Workers report
through their owning actor rather than adding another public addressing layer.

Some host decisions cannot wait behind an actor queue. A policy provider is a
bounded synchronous function over a host query and immutable published state.
It may return a decision, validated directives, and a bounded message batch. It
cannot perform I/O, spawn workers, or inspect mutable actor memory. Invalid or
late results fall back to host policy.

Publications are host-owned immutable latest-state frames written by actors and
read by policy providers. They are not queues and never contain pointers into
mutable actor memory.

## Resource Ceilings

Every actor generation has two complementary forms of backpressure. Item
counts bound queue scheduling, while byte budgets bound retained host and guest
memory. The host rejects a mailbox body, sender identity, worker payload, or
worker result before admitting it when that value exceeds its individual
ceiling. An ownership permit then follows each admitted value through the
generation's queues and is released only when dispatch consumes or drops it.
This prevents a queue containing a bounded number of unusually large values
from bypassing fault containment.

Terminal I/O results use a reserved bounded completion lane rather than the
general actor-operation queue. When that lane is full, its owner stops draining
the completion supervisor until the actor dispatches a result. Backpressure
therefore reaches operation producers while mailbox, lifecycle, and shutdown
commands retain their own admission and scheduling paths. The default lane is
deliberately much larger than the normal outstanding-I/O window, so it contains
pathological completion-producing handlers without throttling ordinary bursts.

Wasmtime stores have linear-memory and table limits. A Component or core-Wasm
generation cannot grow beyond its store limit. Preview 1 workers share their
actor's imported memory, so the module must declare a maximum and that maximum
must fit the generation memory ceiling; every worker store separately retains
the table limit. The defaults are deliberately generous daemon safety limits,
not quotas that ordinary coding plugins should approach.

## Host Responsibilities

Each local actor host owns mailbox, worker, completion, resource scheduling,
actor lifecycle, ABI validation, limits, cancellation, and stale-generation
rejection. Product authority then follows placement:

- the presentation client owns viewport layout, local focus, client input
  policy, UI validation, graphics, and final presentation;
- the backend owns session and managed-application lifecycle, backend resources,
  and lease-authorized hosted Wayland configuration and input;
- the connection router validates cross-placement addresses, capabilities,
  generations, and backpressure.

Plugins own behavior and placement-local state. Host validation of commands is a
product correctness boundary even when the requesting installed plugin is
trusted.

## Lifecycle And Failure

Stopping, crashing, or replacing an actor stops admission, cancels its workers
and operations, removes waits, closes actor-scoped resources, joins host work,
and only then releases the address space. Session-scoped resources remain owned
by the session service and may be reattached by a replacement actor.

Configured hosted actors receive typed `start` and `stop` lifecycle contexts on
the reserved system lane. Dispatch return is not acknowledgement: a hook may
span worker or completion turns and ends only when the guest explicitly
reports lifecycle completion. Start and stop have separate manifest bounds.
Failure or timeout is terminal for that hook, after which the host performs
hard generation teardown and joins every owned resource.

Replacement constructs an unreachable candidate first. The old generation's
stop hook receives the candidate generation, then the host hard-joins the old
generation. The candidate start hook runs while it is still unreachable; only
successful completion publishes it under the stable ID. A failed start is
hard-joined and leaves the ID unavailable. Session rebind uses the same stop
and start hooks around the authority change, while final daemon teardown uses
the `shutdown` stop reason. No old-generation work may mutate state after the
next generation is reachable or after final shutdown returns.

A configured workspace filesystem grant belongs to one actor generation and
the active surface session that selected its capability root. Activating a
different session is an explicit generation-rebind barrier: the host first
stops and joins the old generation, then changes the active session, creates a
new generation rooted at that session's working directory, republishes it under
the stable configured actor ID, and completes its structured start hook. A failed
restart leaves the actor unavailable; it must never preserve access to the
previous session's workspace as a fallback.

Worker preparation is part of that ownership boundary. Once stopping begins, a
preparation which returns late must drop its prepared Store, release its guest
stack and TLS reservation, and report cancellation; it must never begin guest
worker execution. Replacement cannot publish the new generation until that
cleanup has joined.

Diagnostics must expose mailbox pressure, dispatch latency, live and waiting
workers, outstanding operations, wait registrations, resource counts,
cancellation, and terminal failures. One actor failure must remain local to
that actor unless it reveals corruption of a shared host invariant.

## Current Proofs

The Component and Preview 1 runtimes are implementation proofs of this target.
The Component path demonstrates completion I/O and cooperative workers; the
Preview 1 path demonstrates ordinary blocking-looking WASI over shared actor
memory. Their export names and Store layout are not the durable plugin ABI.

`plugins/component-runtime-io` and the runtime-I/O worker in
`plugins/workspace-file-log` exercise the same bounded stream and pollable
contract through both backends. Their nested workload preserves separate
Markdown transcripts containing PATH-selected commands, pipe input and
transformed output, a nonzero child exit, explicit stdin/stdout PTY checks and
interactive traffic, a real loopback request/response, and distinct timer wake
order. The local peer independently records what it received and returned.

`plugins/workspace-file-log` is the user-observable Preview 1 proof. Its module
declares a bounded shared-memory maximum, then performs a real recursive
workspace scan and live filesystem watch. Creating, editing, and removing
files produces corresponding records in
`.manifold/workspace-file-log.log`; the visible file history demonstrates that
normal actor mailbox, worker, shared-memory, and filesystem behavior remains
usable inside the ceilings. The nested workload also creates two invalid-UTF-8
names which lossy conversion would conflate; both real guest logs retain them
as distinct reversible `%FF` and `%FE` paths.

`plugins/component-workspace-watch` proves the same capability-rooted watch
model through the Component backend. Its typed WIT resource is an
actor-generation-owned logical subscription backed by the daemon watcher
service. The guest SDK wrapper projects its notification into the ordinary
worker pollable set, and the plugin maintains a live Markdown history at
`.manifold/component-workspace-watch.md`. The nested proof replaces the actor
generation and observes the old subscription reach zero before the replacement
is published. It also creates two invalid-UTF-8 Unix filenames which would
collapse to the same replacement-character text under lossy conversion; the
artifact shows their distinct native bytes as reversible `\xFF` and `\xFE`
paths.

Host-owned resource teardown retires a watch and its notification as one
ownership transition. An actor event already admitted to the reserved lane may
still carry the retired ID, but it drains as a stale no-op and no longer counts
as pending. Recursive setup tasks are keyed by watch, reaped after completion,
and remain inside the watch ceiling if logical teardown races setup. The
workspace logger's teardown probe repeats real guest watch creation and drop,
then records a later file mutation through the still-live primary watcher.

Current implementation details and runnable tests live in [[extension-host]],
[[plugin-io-interaction-graph]], and [[../reference/wasm-fast-lanes]].

Related:

- [[presentation-client]]
- [[plugin-io-and-resources]]
- [[../concepts/plugin]]
- [[../concepts/session]]
- [[../decisions/0006-plugin-actors-and-workers]]
- [[input-shortcuts]]
- [[layout-actor]]
- [[plugin-ui]]
- [[plugin-graphics]]
- [[plugin-advanced-graphics]]
