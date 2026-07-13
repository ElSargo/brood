---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-13
---

# Pane

A pane is the unit of presentation in a Manifold workspace. Pane state lives behind a layer of indirection rather than in the presentation surface itself.

A pane may be produced by:

- a hosted Wayland app surface
- a browser or terminal pane client
- a plugin or WASM pane app using presentation-rendered components or custom render packets
- a future native producer

Do not use pane to mean process, app, plugin, actor, tab, or native child window. A pane is what the presentation surface arranges and focuses.

The host owns pane chrome, lifecycle, focus integration, and recovery UI. A plugin may own pane-local state and describe content, but if the plugin exits the host should be able to keep the pane shell visible with native recovery controls.

Related:

- [[plugin]]
- [[managed-app]]
- [[presentation-surface]]
- [[../systems/presentation-client]]
- [[../systems/plugin-ui]]
- [[../systems/plugin-graphics]]
- [[../systems/wayland-compositor]]
