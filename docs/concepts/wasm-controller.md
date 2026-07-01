---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-01
---

# WASM Controller

A WASM controller is a WASM module that changes workspace behavior through host APIs. Examples include key handling, layout policy, commands, status publication, automation, or behavior-test control.

Controllers are not compositor plugins. They should interact with Manifold through workspace, session, pane, event, storage, command, and capability APIs rather than through renderer or Smithay internals.

Current runnable lanes are documented in [[../reference/wasm-fast-lanes]].

Related:

- [[../systems/extension-host]]
- [[pane]]
- [[presentation-surface]]
