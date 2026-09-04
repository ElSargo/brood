---
status: target
type: system
authority: product
last-reviewed: 2026-09-04
---

# GPUI Presentation And Managed Plugin UI

GPUI owns Brood's sole native presentation application, window lifecycle,
layout, text, input, accessibility projection, and final GPU presentation.
Plugins publish a versioned Brood semantic UI protocol; no GPUI type, callback,
entity, style object, or lifecycle handle crosses the Component Model boundary.

The complete ownership model, managed vocabulary, state boundary,
reconciliation rules, external-surface and canvas integration, failure limits,
transition phases, and acceptance gates are specified in
[[../../experiments/host-ui-framework]]. That specification is incorporated as
the normative detail of this target. Its former stop conditions remain release
gates and maintenance constraints, not reasons to retain the removed Winit
client.

The normal plugin path is a bounded snapshot/atomic-patch document reduced to
host-owned GPUI elements. The pure renderer remains an escape hatch for canvas
and terminal content. Sessiond-owned DMA-BUFs enter the same GPUI/Blade renderer
as external-surface nodes; shared-memory readback is not a production path.
Brood's actor/session runtime and recursive layout model remain authoritative.

The dependency forks, their provenance, and their maintenance boundary are
recorded in `third_party/README.md`.

Related:

- [[presentation-client]]
- [[plugin-ui]]
- [[plugin-pure-renderer]]
- [[plugin-graphics]]
- [[presentation-transport]]
