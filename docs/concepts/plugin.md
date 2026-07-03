---
status: target
type: concept
authority: product
last-reviewed: 2026-07-03
---

# Plugin

A plugin is a capability-scoped package that extends Manifold without receiving direct authority over compositor, renderer, workspace, or presentation internals.

A plugin may contain:

- one or more asynchronous actors
- zero or more synchronous policy lanes
- pane producers or native component views
- command, action, shortcut, layout, or status registrations
- private worker threads inside the plugin, hidden behind its actor boundary

The host provides the registry, mailbox delivery, capability checks, lifecycle management, native APIs, sync-lane invocation, and presentation integration.

## Async Actors

A plugin actor is the normal unit of plugin execution. Actors communicate by sending messages through host-provided mailboxes. They request host work by sending capability-checked commands instead of mutating Manifold state directly.

Actor mailboxes are the durable communication path. They may queue, apply backpressure, expose diagnostics, and preserve ordering according to the mailbox contract. They are suitable for pane UI updates, background work, indexing, status, command handling, and agent workflows.

## Sync Policy Lanes

A sync policy lane is a special plugin-owned routine that the host may call synchronously when it needs an immediate decision before committing input, focus, mapping, or layout state.

Sync policy lanes are not actors. They do not own threads, drain queues, block on mailboxes, or perform side effects during the host call. They read an immutable state snapshot and return a bounded result containing:

- the answer to the host query
- optional host-validated directives
- a bounded batch of messages to enqueue after the call returns

The host remains the only committer. If a sync lane traps, times out, or returns invalid data, the host falls back to native policy and reports diagnostics.

## Swapchains

A swapchain is the host-owned latest-state publication path between async actors and sync policy lanes.

An actor publishes immutable frames to a default or named plugin-scoped swapchain. The host atomically makes the latest valid frame visible. A sync lane receives a reference, handle, or copied view for the current invocation and reads that latest frame without replaying event history.

Swapchains are not message queues. They have no backlog for the sync lane to drain and cannot block either side. Internal retention may use a linked list, ring, RCU-style root, or another strategy, but the public contract is latest immutable state by generation.

For core WASM, the ABI should avoid assuming that a raw host pointer is meaningful in guest memory. The host may pass a handle, copy a frame into guest memory, or expose a read-only view through an explicit runtime mechanism.

## Host-Owned Boundaries

The host owns:

- final workspace/session/pane commits
- command execution and validation
- shortcut conflict resolution and key routing
- accepted layout validation, hit testing, focus, and input delivery
- native pane chrome, lifecycle, theme, and accessibility
- plugin load, reload, unload, crash, and timeout handling

Plugins own behavior, policy, and local state. They can propose commands, publish state, return sync decisions, and describe pane UI, but the host validates and applies the accepted result.

Related:

- [[../systems/plugin-runtime]]
- [[../systems/input-shortcuts]]
- [[../systems/layout-policy]]
- [[../systems/plugin-ui]]
- [[wasm-controller]]
