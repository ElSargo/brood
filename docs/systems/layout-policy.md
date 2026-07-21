---
status: target
type: system
authority: product
last-reviewed: 2026-07-21
---

# Client-Local Layout Policy

Each presentation client owns an independent WASM layout policy provider. The
provider produces a flat layout specification for that client's pane inventory;
the client validates and executes it locally:

```text
client pane inventory + viewport size + local focus + published plugin state
    -> client-local layout policy provider
    -> flat resolved layout
    -> client validation
    -> WGPU composition, hit testing, focus, and input routing
```

No placement rectangles cross the backend connection. Another presentation
client may show the same session with a completely different policy, viewport,
or visible arrangement.

## Ownership

The presentation client owns:

- its WASM layout policy provider and immutable policy-state input
- final layout validation and canonicalization
- pane bounds, clipping, z order, hit testing, and focus routing
- native fallback when the provider is missing, abstains, traps, or returns an
  invalid candidate
- managed-window size requests derived from accepted pane bounds

The backend owns:

- session and managed-window lifecycle
- stable managed-pane identity and per-window frame production
- lease-authorized Wayland size configuration and application input

The backend does not own viewport placement, z order, chrome, or hit testing.
It receives a managed pane ID and requested logical content size when the
client's accepted layout changes. It configures that Wayland client to the
requested size and returns the resulting per-window content frame. The resize
request deliberately carries no placement coordinate.

## Policy Provider

Layout is a synchronous policy provider because inventory, viewport, focus, or
interactive geometry changes need an immediate local answer before the client
commits its next scene. The provider reads a bounded query plus an optional
immutable publication frame and returns a bounded candidate:

```text
LayoutPolicyInput {
  visible_panes,
  focused_pane,
  presentation_size,
}

LayoutPolicyOutcome {
  layout: ResolvedLayout,
  focus,
}
```

Provider implementation details and persistent split trees remain private WASM
state. They are not part of the client host ABI.

## Flat Resolved Layout

A resolved layout is a flat, client-validated presentation description. It may
contain pane items, placeholders, handles, chrome, and overlays. Each item has
a stable item ID, normalized transform, z value, and input behavior.

Pane items must reference a real pane in the current client inventory and may
appear at most once. A policy may omit a pane to keep it out of this viewport.
Non-pane items let the client report interactive targets without learning the
provider's internal model.

The portable `manifold-layout` crate owns validation, normalized projection,
and hit testing. It does not depend on Smithay, Winit, WGPU, transport code, or
the WASM runtime.

## Pane Identity

The client presents managed panes and client-local WASM panes in one flat
identity namespace. Backend-managed IDs grow upward from the low end;
client-local pane IDs grow downward from the high end. The provider therefore
uses one `PaneId` shape without confusing a local actor pane with a managed
Wayland pane.

## Fallback

The client always retains a deterministic native flat-layout fallback. It is
used when no provider is configured, the provider abstains or fails, or the
candidate does not validate against the current inventory. Fallback keeps the
viewport usable and never asks the backend to invent placement.

Related:

- [[presentation-client]]
- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[plugin-ui]]
- [[../concepts/presentation-surface]]
