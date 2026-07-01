---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-01
---

# Pane

A pane is the unit of presentation in a Manifold workspace. Pane state lives behind a layer of indirection rather than in the presentation surface itself.

A pane may be produced by:

- a hosted Wayland app surface
- a browser or terminal pane client
- a WASM pane app
- a future native producer

Do not use pane to mean process, app, tab, or native child window. A pane is what the presentation surface arranges and focuses.

Related:

- [[managed-app]]
- [[presentation-surface]]
- [[../systems/wayland-compositor]]
