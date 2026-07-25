---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-24
---

# Presentation Surface

A presentation surface is one independent viewport over the available window
catalog. It owns pane visibility, placement, dragging, animation, local focus
routing, hit testing, native chrome, and final input routing for what it
currently shows. Session membership is metadata the client may use when
choosing a view; it is not a backend mapping boundary. Two presentation
surfaces may show different subsets or arrange the same windows differently.

A presentation surface runs in a portable presentation client with its own
actors, local state, recursive layout-lease tree, WGPU device, and final
renderer. It receives stable window identities with their session membership
before frame readiness and subscribes only to the frame streams it needs. A
layout may place an available window immediately; the client shows a
placeholder until that window's frame is mapped. One presentation lease holder
controls hosted Wayland sizing and primary application input. The session
server has no in-process presentation endpoint. See
[[../systems/presentation-client]].

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
