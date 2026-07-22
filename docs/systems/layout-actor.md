---
status: target
type: system
authority: product
last-reviewed: 2026-07-22
---

# Client-Local Layout Actor

Each presentation client owns its layout actors. A layout actor is an ordinary
mailbox-driven actor, using the same execution model as an actor in the
in-process presenter. Layout does not have a synchronous provider lane.

```text
client inventory, viewport, or focus changes
    -> `layout-state` message to subscribed actors
    -> actor mailbox epoch and private actor state
    -> ordinary host outbox message: `set-resolved-layout`
    -> client validation and commit
    -> WGPU composition, hit testing, focus, and input routing
```

Key handling and UI commands use the same route. A synchronous key decision may
consume an event and address a `layout-action` message to a layout actor, but it
cannot submit layout directly. The actor decides how that action changes its
private split tree, focus, fullscreen state, or other presentation model and
then emits a candidate resolved layout through its normal outbox.

No placement rectangles cross the backend connection. Another presentation
client may show the same session with a different actor state, viewport, or
visible arrangement.

## Ownership

The presentation client owns:

- layout actor instances, mailboxes, generations, and event subscriptions;
- publication of bounded `layout-state` snapshots after local state changes;
- final layout validation and canonicalization;
- pane bounds, clipping, z order, hit testing, and focus routing;
- deterministic native fallback while no valid actor result is available;
- managed-window size requests derived from accepted pane bounds.

The layout actor owns its internal layout model and the decision to emit a new
candidate. Its state is not part of the host ABI.

The backend owns session and managed-window lifecycle, stable managed-pane
identity, per-window frame production, and lease-authorized Wayland size
configuration and application input. It does not own viewport placement, z
order, chrome, or hit testing. Resize requests carry pane identity and requested
logical content size, never placement coordinates.

## Messages

The host sends `layout-state` after pane inventory, viewport size, application
identity, or focus changes. Delivery is queued and bounded like any other actor
message; the client remains responsive while an actor processes it.

Other actors and the key lane may address ordinary commands such as
`layout-action` to the layout actor. A result reaches the host only as a
`set-resolved-layout` message in the actor's ordinary host outbox. The host
rejects direct layout submission from the synchronous key lane.

There is no request/return pairing requirement. Revisions make state snapshots
observable, while normal actor mailbox ordering determines processing order.
An actor may coalesce work or emit more than one update. The host always
validates a candidate against its current pane inventory before committing it.

## Flat Resolved Layout

A resolved layout is a flat, client-validated presentation description. It may
contain pane items, placeholders, handles, chrome, and overlays. Each item has
a stable item ID, normalized transform, z value, and input behavior.

Pane items must reference a real pane in the current client inventory and may
appear at most once. An actor may omit a pane to keep it out of this viewport.
Non-pane items let the client expose interactive targets without learning the
actor's internal model.

The portable `manifold-layout` crate owns validation, normalized projection,
and hit testing. It does not depend on Smithay, Winit, WGPU, transport code, or
the WASM runtime.

## Pane Identity

The client presents managed panes and client-local WASM panes in one flat
identity namespace. Backend-managed IDs grow upward from the low end;
client-local pane IDs grow downward from the high end. Actors therefore use one
pane ID shape without confusing a local pane with a managed Wayland pane.

## Fallback And Failure

The client immediately uses deterministic native flat layout when no layout
actor is configured or the accepted actor layout no longer matches current
inventory. It also falls back after an invalid candidate. A later valid actor
outbox message can become authoritative again.

Actor failure follows normal generation failure and replacement rules. It does
not introduce a layout-specific failure channel or block the Winit event loop.

Related:

- [[presentation-client]]
- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[plugin-ui]]
- [[../concepts/presentation-surface]]
