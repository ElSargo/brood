# 0006: Actors Are Fault Domains And Workers Are Private Tasks

## Status

Accepted as the target plugin execution model.

## Context

The earlier plugin model made actors the normal unit for both public message
handling and concurrent background work. It allowed unspecified internal
threading, but did not define how ordinary WASI calls, completion-based actor
I/O, shared memory, or cheap thread-like concurrency fit together.

Creating another actor is appropriate when work needs independent isolation,
addressing, capabilities, or failure handling. It is unnecessarily expensive
and awkward when plugin code only needs to overlap I/O while sharing ordinary
language state and synchronization.

## Decision

An actor is the public plugin fault-isolation, addressing, resource-ownership,
and lifecycle domain. It owns an address space, mailbox slots, resources,
publications, and failure state. The host may leave it dormant while waiting
for its next event, but once invoked its completion-driven dispatcher must not
suspend or block on I/O or synchronization.

A worker is a private execution context scoped to an actor. Workers share their
actor's memory, state, resources, and capabilities. They have no public address
or mailbox and cannot outlive the actor. They may use blocking-looking standard
WASI calls because the runtime suspends only the calling worker while the host
performs the operation asynchronously.

The actor-facing I/O contract is completion based and delivered through message
passing. Workers may use that contract directly or use ordinary synchronous
language and libc APIs projected over the same completion engine.

The initial worker contract is cooperative concurrency. It does not promise
multicore parallelism. Parallel shared-memory execution may be added later only
through an explicit API and capability with correspondingly stronger
synchronization requirements.

Policy providers remain separate bounded synchronous routines. They read
host-owned immutable publications and cannot block, spawn workers, perform I/O,
or inspect mutable actor memory.

Installed plugins inside one session are trusted user code. Actor address-space
separation remains essential for crash containment and clear ownership, but is
not advertised as an adversarial security boundary. Authority isolation belongs
to the enclosing local, containerized, or remote session environment.

## Consequences

- Actors remain the only host-visible message and fault-isolation boundary.
- Worker creation is cheaper than actor creation because it reuses the actor's
  address space, resource table, capabilities, and lifecycle.
- Plugin SDKs need distinct actor-dispatch and worker contexts so blocking
  operations are not accidentally exposed on the actor path.
- Guest-owned channels, locks, joins, and condition variables may coordinate
  workers through host notification/poll primitives; the actor receives events
  rather than waiting on those structures.
- Actor shutdown and hot replacement must cancel workers and in-flight I/O.
- Runtime diagnostics and limits must cover worker count, outstanding I/O,
  completion backlog, starvation, cancellation, and worker state memory.
- The durable ABI should use standard WASI plus Manifold-specific WIT APIs rather
  than a bespoke replacement for normal file and stream I/O.
- Normal trusted-plugin operation does not add per-I/O approval prompts. Host
  bridges and authority are configured at the session boundary.
- The current core-WASM epoch ABI remains a prototype and is not the target
  execution contract.

The detailed resource and scheduling contract is specified in
[[../systems/plugin-io-and-resources]].

## Verification Requirement

Before the worker API is considered viable, a real WASI guest must block a
worker in ordinary `read`, continue handling actor mailbox traffic within the
actor latency budget, resume the worker after input arrives, and cancel the
worker cleanly when the actor unloads.
