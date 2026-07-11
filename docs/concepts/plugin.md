---
status: target
type: concept
authority: product
last-reviewed: 2026-07-11
---

# Plugin

A plugin is a capability-scoped package that extends Manifold without receiving
direct authority over compositor, renderer, workspace, or presentation
internals.

A plugin may contain:

- one or more actors
- actor-scoped workers
- zero or more policy providers
- pane producers or native component views
- command, action, shortcut, layout, or status registrations

The host provides actor addressing, mailbox delivery, worker scheduling,
completion delivery, WASI resources, capability checks, lifecycle management,
policy-provider invocation, and presentation integration.

## Actors

An actor is the public isolation, addressing, and lifecycle unit of plugin
execution. It owns an address space, durable local state, a capability/resource
table, publications, and a globally addressable mailbox with optional named
slots.

Actors communicate with other actors and host services through host-provided
mailboxes. They request host work through capability-checked commands instead of
mutating Manifold state directly.

An actor may remain dormant while the host waits for its next mailbox or other
event. Once dispatch begins, its dispatcher must not suspend or block on I/O,
locks, channels, joins, or other unbounded work. It submits completion-based
operations, handles completion messages, and delegates blocking-looking or
long-running work to workers.

## Workers

A worker is a private execution context inside an actor. It shares the actor's
memory, state, resources, and capabilities, but it has no actor address, public
mailbox, publication identity, or independent lifecycle.

Workers may use ordinary blocking-looking WASI calls such as `read` and `write`.
The host implements those calls asynchronously and suspends only the calling
worker. Workers may instead use the same completion-based APIs as the actor.

Workers coordinate with sibling workers through guest-owned shared-memory data
structures, atomics, and host scheduling notifications. Workers communicate
with their actor through actor-local events or shared queues/snapshots followed
by a coalesced notification. The actor does not wait on worker mutexes or
channels.

Workers are scoped to their actor. Stopping, replacing, or crashing an actor
cancels its workers and outstanding I/O before releasing the actor's address
space. The initial worker contract is cooperative concurrency, not multicore
parallelism.

Use a worker for cheap shared-state concurrency. Use another actor when work
needs an independent address, memory space, capability set, failure boundary, or
lifecycle.

## I/O

The runtime exposes standard WASI interfaces backed by Manifold's capability
and completion engine. Ordinary POSIX-like library calls should work in worker
context when the corresponding resource has been granted. The actor-facing
default remains a completion protocol delivered through message passing.

Manifold-specific APIs extend WASI with actors, mailbox slots, workers,
publications, and capability-checked workspace commands. They do not replace
portable file, stream, socket, clock, and process interfaces with bespoke
equivalents.

## Policy Providers

A policy provider is a special plugin-owned routine that the host may call
synchronously when it needs an immediate decision before committing input,
focus, mapping, or other host state.

Policy providers are not actors or workers. They do not own threads, drain
queues, spawn workers, block, perform I/O, or mutate host state during the call.
They read an immutable state snapshot and return a bounded result containing:

- the answer to the host query
- optional host-validated directives
- a bounded batch of messages to enqueue after the call returns

If a policy provider traps, times out, or returns invalid data, the host falls
back to native policy and reports diagnostics.

## Publications

A publication is the host-owned latest-state path between mutable actor state
and policy providers. An actor publishes immutable frames to a default or named
publication. Workers may prepare publication data, but publication ownership and
identity remain with the actor.

Publications are not message queues or views into shared actor memory. They have
no backlog to drain and cannot block either side. The host copies or validates
owned values, offsets, and handles before atomically making a frame visible; a
published frame cannot contain pointers into actor or worker memory.

## Host-Owned Boundaries

The host owns:

- final workspace, session, and pane commits
- command execution and validation
- shortcut conflict resolution and key routing
- accepted layout validation, hit testing, focus, and input delivery
- native pane chrome, lifecycle, theme, and accessibility
- plugin load, reload, unload, crash, timeout, and resource limits

Plugins own behavior, policy, and local state. Their actors and workers can
propose commands, publish state, return policy decisions, and describe pane UI,
but the host validates and applies accepted results.

Related:

- [[../systems/plugin-runtime]]
- [[../systems/plugin-io-and-resources]]
- [[../decisions/0006-plugin-actors-and-workers]]
- [[../systems/input-shortcuts]]
- [[../systems/layout-policy]]
- [[../systems/plugin-ui]]
- [[wasm-controller]]
