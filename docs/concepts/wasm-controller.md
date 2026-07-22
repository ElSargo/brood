---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-10
---

# WASM Controller

A WASM controller is a WASM-backed [[plugin]] role that changes workspace behavior through host APIs. Examples include key handling, layout actors, commands, status publication, automation, or behavior-test control.

Controllers are not compositor plugins. They should interact with Manifold through workspace, session, pane, event, storage, command, mailbox, publication, and capability APIs rather than through renderer or Smithay internals.

Normal controller behavior should run through addressed actors. Actor
dispatchers remain completion-driven and delegate blocking-looking I/O or
shared-state concurrent work to private workers. A controller may also declare
privileged policy providers for immediate host decisions such as key routing or
layout, but those providers read immutable published state and return bounded
decision envelopes instead of mutating host state directly.

Current runnable lanes are documented in [[../reference/wasm-fast-lanes]]. The target architecture is documented in [[../systems/plugin-runtime]], [[../systems/input-shortcuts]], and [[../systems/layout-actor]].

Related:

- [[../systems/extension-host]]
- [[../systems/plugin-runtime]]
- [[../systems/input-shortcuts]]
- [[../systems/layout-actor]]
- [[pane]]
- [[presentation-surface]]
