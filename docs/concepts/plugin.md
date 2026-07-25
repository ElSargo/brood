---
status: target
type: concept
authority: product
last-reviewed: 2026-07-21
---

# Plugin

A plugin is trusted session software packaged to extend Brood. Installing a
plugin is equivalent to choosing to run it as the session user; the plugin
runtime is a portability, lifecycle, and fault-isolation boundary rather than
an adversarial sandbox.

A package may contain actors, policy providers, publications, pane producers,
and action or service registrations. Actors and policy lanes declare whether
they run in the session backend or in a presentation client. Presentation
actors own endpoint-local interaction and UI state; session actors own backend
or explicitly coordinated session behavior. The runtime does not transparently
migrate mutable actors between those placements.

## Execution Roles

An **actor** is the public identity, address space, mailbox, state, and failure
domain. It is dormant between events. Once invoked, its dispatcher handles one
event and returns without waiting on I/O or worker synchronization.

A **worker** is a private thread-like task inside an actor. It shares that
actor's memory and resources, may use blocking-looking WASI calls, and suspends
independently. It has no public address and cannot outlive the actor.

A **policy provider** answers a bounded immediate host query from immutable
published state. It cannot perform I/O, inspect mutable actor memory, or block.

Use workers for concurrent work over shared state. Use another actor for an
independent address, address space, failure domain, or lifecycle.

## Communication And I/O

Actors communicate through bounded host mailboxes. Same-runtime messages remain
local; explicitly addressed session-to-presentation and
presentation-to-session messages cross the presentation connection. The
connection authenticates the sender placement and actor identity rather than
trusting identity fields in the message body. Actors submit I/O and later
receive typed completion events. Workers may use the same operations through
ordinary WASI calls and worker `select!`; only the calling worker waits.

Workers coordinate through guest-owned shared-memory structures plus host
notifications. The host schedules wakeups but does not define plugin mutex,
channel, semaphore, or queue layouts. An actor observes shared changes through
events and bounded `try_*` operations; it never waits for a worker lock.

Publications are host-owned immutable latest-state frames. They let policy
providers read actor-derived state without entering mutable actor memory.

## Trust Boundary

Plugins inside one session are trusted peers but remain isolated from each
other for crash containment and comprehensible ownership. The enclosing local,
containerized, or remote session determines available filesystem, process,
network, device, credential, and host-bridge authority.

Capability declarations describe expected resources and budgets. They support
configuration and audit but are not presented as protection against a malicious
installed plugin or a native subprocess it launches.

The host still owns scheduler and product invariants: actor lifecycle, resource
limits, final workspace and presentation commits, input routing, layout
validation, pane lifecycle, and bridges across the session boundary.

Related:

- [[../systems/presentation-client]]
- [[../systems/plugin-runtime]]
- [[../systems/plugin-io-and-resources]]
- [[session]]
- [[../decisions/0006-plugin-actors-and-workers]]
- [[../systems/plugin-ui]]
- [[../systems/plugin-graphics]]
- [[../systems/plugin-advanced-graphics]]
- [[wasm-controller]]
