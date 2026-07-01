---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-01
---

# Presentation Surface

A presentation surface presents one session at a time. It decides pane visibility, placement, dragging, animation, focus routing, and input forwarding for what is currently visible.

The current prototype's session process also owns the MVP presentation endpoint. A future architecture may split presentation into a separate process.

Presentation state is not pane state. A presentation surface may save layout or visibility preferences into the workspace, but the pane remains owned by its producer or session state.

Related:

- [[session]]
- [[pane]]
- [[../systems/runtime]]
- [[../systems/wayland-compositor]]
