---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-03
---

# Presentation Surface

A presentation surface presents one session at a time. It owns pane visibility, placement, dragging, animation, focus routing, hit testing, native chrome, and input forwarding for what is currently visible.

The current prototype's session process also owns the MVP presentation endpoint. A future architecture may split presentation into a separate process.

Presentation state is not pane state. A presentation surface may save layout or visibility preferences into the workspace, but the pane remains owned by its producer or session state.

Plugins may influence presentation through host-validated requests and layout policy providers. They do not directly own final rendering, hit testing, focus routing, or accepted layout commits.

Related:

- [[session]]
- [[pane]]
- [[../systems/layout-policy]]
- [[../systems/input-shortcuts]]
- [[../systems/plugin-ui]]
- [[../systems/runtime]]
- [[../systems/wayland-compositor]]
