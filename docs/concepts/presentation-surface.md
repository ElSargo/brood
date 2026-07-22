---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-13
---

# Presentation Surface

A presentation surface is one independent viewport onto one session. It owns pane visibility, placement, dragging, animation, local focus routing, hit testing, native chrome, and final input routing for what it currently shows. Two presentation surfaces may arrange the same session differently; the backend does not own one canonical viewport layout.

The target presentation surface runs in a portable presentation client with its own client-side actors, local state, WGPU device, and final renderer. It discovers backend windows cheaply and subscribes only to the frame streams it needs. One optional presentation lease holder controls hosted Wayland sizing and primary application input for a session. See [[../systems/presentation-client]].

The current prototype's session process also owns the MVP Winit/GLES presentation endpoint. The target separates that endpoint behind a typed in-process boundary before moving it into a client process.

Presentation state is not pane state. A presentation surface may save layout or visibility preferences locally or explicitly coordinate them into session storage, but the pane remains owned by its producer or session state.

Plugins may run presentation actors locally and influence presentation through validated actor messages. The presentation client still owns final rendering, hit testing, focus routing, and accepted viewport commits.

Related:

- [[session]]
- [[pane]]
- [[../systems/presentation-client]]
- [[../systems/layout-actor]]
- [[../systems/input-shortcuts]]
- [[../systems/plugin-ui]]
- [[../systems/runtime]]
- [[../systems/wayland-compositor]]
