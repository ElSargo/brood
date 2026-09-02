---
status: target
type: system
authority: product
last-reviewed: 2026-09-02
---

# Plugin Runtime

Brood treats an installed and approved plugin as trusted user code in its
execution environment. Actors may run in the session backend or in a
presentation client according to explicit package placement. The runtime
provides fault isolation, scheduling, lifecycle, and a portable I/O environment;
it does not claim that a useful coding plugin is an adversarial security sandbox.
A remote peer cannot authorize code execution merely by sending a WASM
artifact. The destination realm owns installation policy and capabilities; see
[[artifact-distribution]].

The complete model has four execution roles:

| Role | Purpose | May wait on I/O? | Isolation |
| --- | --- | --- | --- |
| Actor | Public identity, mailbox, state, and lifecycle | Only while dormant between events | Own address space |
| Worker | Private concurrent work inside an actor | Yes; only that worker suspends | Shares its actor's memory |
| Policy provider | Immediate bounded host decision | No | Reads immutable published state |
| Render facet | Host-timed conversion of live actor state and exact pane parameters into graphics | No | Exclusive phase in its actor instance |

The detailed I/O and resource protocol is specified in
[[plugin-io-and-resources]].

## Guest SDK

Component Model plugins depend on `crates/brood-plugin-sdk`. It is the
canonical generation point for `crates/brood-wasm/wit/brood-plugin.wit`, so a
host-contract change is compiled once and every plugin implements the same
`Guest` types. The SDK also owns the small mechanisms whose invariants should
not vary by plugin: explicitly addressed actor commands, stable input bits,
bounded label preparation, draw-packet submission and resource retirement, and
worker-only notification, timer, stream, child, and subprocess handling.

The SDK remains below plugin policy. It does not choose actors, capabilities,
mailbox formats, supervision trees, retry policy, process allowlists, terminal
emulation, editor protocols, widget identity, or layout. The actor dispatcher
still returns commands immediately; only `worker-run` code may use the SDK's
waiting I/O helpers.

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
- a plugin crash or runaway actor or worker must not tear down the session or
  starve its peers;
- the host validates ABI data and owns compositor, renderer, and lifecycle
  invariants.

These are fault-containment and correctness guarantees. They protect the system
from bugs, stale work, and resource exhaustion, not from a malicious installed
plugin with legitimate session authority.

Authority is bounded only by the enclosing session environment. A direct local
session has the full authority of its user. Container and remote sessions add a
clear environment boundary which defines the available filesystem mounts,
processes, devices, network, credentials, and host bridges. Plugins within any
one environment are trusted peers. See [[../../concepts/session]].

## Runtime Shape

```text
Session backend                         Presentation client
├── daemon actor                        ├── presentation actor
│   └── worker                          │   ├── worker
├── session actor                       │   └── worker
│   └── worker                          ├── local key decision + layout actor
├── backend policy/service lanes
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

## Realm Init Actors

The daemon, every surface session, and every presentation client may have one
blessed init actor. Each is an ordinary Component Model actor with an ordinary
Store, mailbox, lifecycle, resource table, and failure boundary. The host
creates it as part of realm creation and delivers its normal start lifecycle
event before admitting the rest of that realm's actor topology.

The daemon realm exists once per server process and is not attached to an
active surface session. It owns backend policy and services whose identity must
survive session creation, activation, and closure. A server with no surface
sessions may therefore still run its daemon init actor. Session roots are
created and destroyed with their individual surface sessions; the presentation
root is created and destroyed with its client process.

The blessing adds one capability: the init actor may request that the local host
start, replace, or stop a named child actor. Child paths are capability-rooted
inside the init package rather than ambient host paths. The host reports typed
started, stopped, and failed child events back to the init actor. Restart
policy, backoff, dependency order, optional services, and topology therefore
remain guest behavior rather than a host-owned desired-graph format.

A child start request selects a package-declared capability profile. The host
resolves that profile against the package grant and enclosing session before
constructing the child runtime. The root may choose among declared profiles but
cannot mint filesystem, process, PTY, network, credential, or presentation
authority by spelling an undeclared capability in a message.

A surface-session root may also request a named Wayland app with an argv. The
host binds that process and its pane to the root's session and reports typed
started, stopped, and failed events. Choosing `foot` as the default terminal is
therefore behavior in the default root artifact, not compositor startup policy.

Only the realm's root receives actor management, and only a surface-session
root receives app launch. A child cannot replace the root or stop an actor it
does not own, and actors in one placement cannot use init authority in another
placement. A client init actor creates client-local actors; a session init actor
creates actors and apps in its own surface session; a daemon init actor creates
backend actors whose lifecycle is independent of every session.

A presentation root supplies one explicit `presentation-scope` when it starts
each child: `global` or one backend-qualified surface session. The host retains
this immutable scope for pane filtering, workspace authority, cross-realm
message validation, and explicit session teardown. Actor names are labels, not
an ownership convention, and changing the active session never changes an
existing child's scope.

The default daemon root is the session storage manager. It serializes session
volume and managed-worktree acquisition, records occupation, and decides which
idle resources are eligible for retention or cleanup. Its workers perform
allow-listed policy operations and persist metadata under the managed XDG data
root. Native host mechanisms perform validated namespace, mount, reflink,
export, and removal work. The host remains responsible for validating actor
results and attaching or activating surface sessions; it does not absorb
storage policy into the session picker. The complete storage and process
boundary is specified in [[session-isolation]].

The host retains mechanism and recovery responsibility. It validates identities
and package-root paths and enforces generation and resource limits. Scheduler
budgets are cooperative yield points, never actor termination conditions. The
host hard-stops Stores, workers, operations, native children, streams,
mailboxes, and stale generations only for an explicit stop, replacement, or
realm exit. Recovery startup may explicitly omit the init actor so a broken
root cannot make the realm uninspectable.

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

A presentation actor may explicitly opt into a bounded presentation batch of
up to sixteen ordinary events. The default, including every new replacement
generation, remains one event per dispatch. A batch is one actor event and one
presentation-lane scheduling quantum: it is a FIFO prefix of reliable
transitions, never crosses a render hint, and reports the maximum reduced
watermark only after the whole dispatch returns within the aggregate 64-command
ceiling. Render hints, paste payloads, and image payloads remain singleton
events. Other actor lanes keep their round-robin opportunity after the one
bounded batch rather than after each member. Opt-in changes delivery
granularity, so an actor must preserve the same command ordering it would have
produced from the singleton prefix.

This rule is about scheduler responsiveness, not security. It keeps lifecycle,
input, cancellation, and mailbox traffic moving while background work waits.
The host enforces peer fairness by yielding CPU-bound guest execution.
Exhausting an epoch or fuel quantum does not fail or restart the actor;
execution resumes on a later scheduler turn.

The epoch clock is shared by all local actor engines and is armed while the host
polls guest execution, with a short grace period to coalesce adjacent polls. A
dormant actor or a worker waiting on input or I/O does not create periodic
process wakeups. Once guest code is runnable, the configured wall-clock yield
cadence applies immediately, so idle power behavior does not trade away
cancellation or peer-scheduling latency.

Use another actor when work needs an independent address, memory space, failure
domain, or lifecycle.

## Render Facets

A presentation actor may expose an optional host-driven render facet as defined
by [[plugin-pure-renderer]]. The presentation client invokes that export in the
actor's existing instance with exact host-owned pane geometry. The facet has no
mailbox, public address, workers, I/O, commands, publications, or independent
product lifecycle.

The host waits until an actor dispatch has returned and worker execution is
suspended at a host-controlled safe boundary, then runs the render phase
exclusively before ordinary mailbox work resumes. An epoch yield in the middle
of a guest handler is not reentrant and remains the same active turn. Rendering
reads live actor state without serialization or copying. Render requests are
host-priority work at a safe turn boundary, not messages inserted into the
actor's FIFO.

The host masks effectful imports during the phase, while internal semantic
purity remains a trusted plugin contract. Cooperative guest yield preserves
peer progress but does not let the same actor process another event
concurrently.
Hard interruption cannot restore same-instance state and therefore belongs to
generation failure or teardown, not normal latest-wins rendering. Stopping or
replacing the generation destroys its pending requests and accepted-generation
outputs with the rest of the actor.

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
portable facilities. Brood APIs add actor addressing, workers,
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

### Terminal plugin composition

The first-party terminal is an ordinary bundled Component plugin, not a host
subsystem. Its session actor owns the allowlisted shell PTY, parser, terminal
state, and scrollback. Its presentation actor owns pane-local input, native text
layout handles, and canvas packets. The host supplies only the same bounded
PTY/process, stream readiness, presentation input, text, graphics, and
diagnostic facilities available to other plugins.

The session actor parses output in a worker and publishes revisioned snapshots.
It coalesces a short burst of PTY reads before publishing and chunks a snapshot
to stay below mailbox limits; it does not forward one actor message per native
read. The presentation actor assembles a complete revision, supersedes stale
render work, and emits a retained canvas packet. Input dispatch only enqueues
bounded writes or work and never waits for the PTY or renderer.

Terminal escape handling, the screen grid, scrollback, selection, and
keybindings remain guest code. The runtime does not expose descriptors,
arbitrary `ioctl`, signal numbers, termios internals, compositor objects, or GPU
objects to support it.

## Messaging, Policy, And Publications

Mailboxes are the durable communication path between actors, host services,
panes, and agents. Mailboxes are bounded, reject or expose backpressure, and
carry sender, target, slot, generation, and causal identity. Workers report
through their owning actor rather than adding another public addressing layer.

Cross-placement delivery is bidirectional, addressed by realm, and
connection-scoped. A presentation actor addresses a backend-qualified daemon
realm or session realm. A session-associated presentation actor may address
only its own backend and session; a presentation-global actor may address a
known backend realm when its application protocol requires it. Daemon and
session actors may reply to a presentation actor on the subscribed connection.
The transport authenticates the source placement and never infers a backend or
realm from an actor name.

Disconnecting a presentation removes its return route without changing a
daemon or session actor's identity or lifecycle. Subscription, replay,
snapshot, and unsubscription remain application protocols carried in mailbox
messages; the host does not impose a special observer API. DBus may bootstrap a
local presentation connection and remains a public control surface, but it is
not an internal actor-to-actor messaging workaround.

An actor may also place a bounded message in its local host outbox. The host
attaches the actor generation as authenticated origin before interpreting the
message, so capabilities such as layout leases remain ordinary asynchronous
actor traffic without trusting a sender identity supplied by guest bytes.

Some host decisions cannot wait behind an actor queue. A policy provider is a
bounded synchronous function over a host query and immutable published state.
It may return a decision, validated directives, and a bounded message batch. It
cannot perform I/O, spawn workers, or inspect mutable actor memory. Invalid or
late results fall back to host policy.

Publications are host-owned immutable latest-state frames written by actors and
read by policy providers. They are not queues and never contain pointers into
mutable actor memory.

Presentation delivery makes the same state/transition distinction. Consecutive
pointer positions and pane sizes for one pane retain only the newest value
before guest dispatch, while keys, text, buttons, and focus transitions remain
lossless. A failed init-owned presentation actor is removed from pane routing
before queued local messages are delivered, so one closed child mailbox cannot
terminate the presentation client while the init actor replaces it.

The batch event and opt-in command are a coordinated structural change to the
experimental `brood:plugin@0.1.0` world. Brood currently rebuilds every bundled
component atomically with its host; stale components are expected to fail
component type matching rather than being treated as backward compatible.

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

Implementation evidence is recorded in [[../../progress/plugin-runtime-proofs]].

Related:

- [[presentation-client]]
- [[plugin-io-and-resources]]
- [[../concepts/plugin]]
- [[../../concepts/session]]
- [[../../decisions/0006-plugin-actors-and-workers]]
- [[input-shortcuts]]
- [[layout-actor]]
- [[plugin-ui]]
- [[terminal-plugin]]
- [[plugin-graphics]]
- [[render-scheduling]]
- [[plugin-advanced-graphics]]
