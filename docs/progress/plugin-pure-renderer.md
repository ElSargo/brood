---
status: active
type: progress
authority: implementation
last-updated: 2026-09-05
---

# Snapshot Facet Architecture Review

Target: [[../targets/systems/plugin-pure-renderer]]

The architecture review selected immutable application-view publication followed
by host-invoked render and input facets. This supersedes the same-instance
live-mutable-state render phase proposed earlier. No runtime implementation or
ABI migration is claimed by this documentation change.

The actor publishes a coherent view; render produces concrete draw commands and
an immutable interaction snapshot; the host commits them together. Input uses
that frame's retained interaction data plus explicit sequence state and returns
semantic actor events. Layout remains arbitrary plugin code, not deferred host
layout expressions in packets.

Code review found actual actor/worker shared-state usage in terminal-session,
neovim-client, neovim-files, and coding-agent. Session-switcher has a no-op worker;
terminal-ui workers wait on timers and return payloads. Existing worker sharing
is not removed by the chosen target. Published views must not expose mutable
aliases, and facets must not depend on freezing those workers.

Next work is a storage/executor prototype, not immediate product migration.
Measure snapshot preparation, atomic publication, acquisition, transfer, retained
bytes, and invocation under busy actor/worker load. Resolve ABI representation,
reclamation, isolation/failure scope, and input admission capacity before freezing
interfaces. Near-instant publication is a root-exchange goal, not evidence that
snapshot construction is free.

Verification for this revision: documentation consistency searches and
`git diff --check`; no build or behavior tests, because no runtime code changed.
Existing historical progress records and alternative experiments remain history,
not updated claims that this target has already shipped.
