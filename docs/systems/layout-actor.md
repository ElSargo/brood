---
status: target
type: system
authority: product
last-reviewed: 2026-07-24
---

# Client-Local Layout Actor

Each presentation client owns a host-validated tree of layout leases. Every
lease is held by an ordinary mailbox-driven actor using the same execution model
as an actor in the in-process presenter. Layout does not have a synchronous
provider lane.

```text
root viewport lease
    -> wrapper lease: sidebar pane + `rest` child slot
        -> wrapper lease: top bar pane + `rest` child slot
            -> leaf lease: user layout panes
    -> recursive composition into one flat resolved layout
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
- the layout-lease tree, holder identity, generation, and pane delegation;
- publication of bounded, lease-scoped `layout-state` snapshots after local
  state changes;
- final layout validation and canonicalization;
- recursive transform, clipping, and paint-order composition;
- pane bounds, clipping, z order, hit testing, and focus routing;
- deterministic native fallback while no valid actor result is available;
- managed-window size requests derived from accepted pane bounds.

Each layout actor owns its internal layout model and the decision to emit a new
candidate for its lease. Its state is not part of the host ABI.

The backend owns session and managed-window lifecycle, stable managed-pane
identity, per-window frame production, and lease-authorized Wayland size
configuration and application input. It does not own viewport placement, z
order, chrome, or hit testing. Resize requests carry pane identity and requested
logical content size, never placement coordinates.

## Layout Leases

A layout lease is an exclusive, revocable capability over one node of the
presentation layout tree. The client host always mints and records the lease.
An init actor or parent layout actor may choose a child actor and request
delegation, but it cannot create authority the host does not know about.

A lease grant binds:

```text
lease identity and generation
holder actor and actor generation
parent slot, or the root viewport
owned pane set
delegated pane set
inventory revision
optional expiry or liveness policy
```

The first implementation permits at most one delegated child named `rest`.
That shape covers wrappers such as sidebars, top bars, palettes, and floating
plugin panes without defining pane-partition policy for multiple children.
Future named children extend the same lease tree by delegating disjoint pane
sets.

Pane ownership is exclusive. A layout fragment may reference only panes owned
by its lease. It cannot enumerate or reposition panes delegated to `rest`.
Lease-scoped queries expose the allocated viewport, owned pane inventory,
focused descendant, application identity needed by policy, and child summary.
They do not implicitly grant access to unrelated presentation state.

Actor-to-actor delegation is therefore semantic rather than an authority
bypass: the parent requests a delegation and the host returns the child grant.
Every authoritative query and result carries the lease identity, lease
generation, and input revision. The host also verifies that the message sender
is the recorded lease holder.

## Messages

The host sends `layout-state` after lease grant, pane inventory, viewport size,
application identity, or focus changes. Delivery is queued and bounded like any
other actor message; the client remains responsive while an actor processes it.

Other actors and the key lane may address ordinary commands such as
`layout-action` to the layout actor. A result reaches the host only as a
`set-resolved-layout` message in the actor's ordinary host outbox. The host
rejects direct layout submission from the synchronous key lane.

There is no synchronous request/return pairing requirement. Revisions make
state snapshots observable, while normal actor mailbox ordering determines
processing order. An actor may coalesce work or emit more than one update. The
host rejects stale lease generations and results computed from an obsolete
inventory revision.

A layout result contains the lease token, input revision, locally normalized
items, an optional `rest` child transform and local paint position, and an
optional focus request. The actor never supplies its insertion point in the
global tree; the lease determines where the host inserts the fragment.

The default session-switcher composition is a concrete instance of this model.
The blessed init child claims the root, places its pane in a right sidebar, and
delegates the remaining pane set to the `workspace` actor. When the sidebar is
hidden, the same root fragment expands `rest` to the full viewport, preserving
the workspace actor and its layout state.

## Recursive Composition

Each fragment uses coordinates local to its lease viewport. The client
recursively composes the `rest` transform with its child's transforms, clips the
child subtree to the slot, and canonicalizes hierarchical paint order. A parent
may reserve space by shrinking `rest` or place its own pane over a full-size
`rest` child.

Paint order is scoped. Parent items and the child slot are ordered locally, then
the client expands the child at that position. Arbitrary child z values cannot
escape above a parent overlay. Item identities are lease-local until the client
canonicalizes the flat result.

The outermost valid focus request wins. If a wrapper does not request focus,
the accepted request from its child remains eligible. The client still verifies
that the resulting pane is visible and belongs to the submitting subtree.

## Flat Resolved Layout

A resolved layout is a flat, client-validated presentation description. It may
contain pane items, placeholders, handles, chrome, and overlays. Each item has
a stable item ID, normalized transform, z value, and input behavior.

Pane items must reference a real pane in the current client inventory and may
appear at most once. An actor may omit a pane to keep it out of this viewport.
Non-pane items let the client expose interactive targets without learning the
actor's internal model.

The portable `manifold-layout` crate owns validation, normalized projection,
recursive lease-fragment composition, and hit testing. It does not depend on
Smithay, Winit, WGPU, transport code, or the WASM runtime.

## Pane Identity

The client presents managed panes and client-local WASM panes in one flat
identity namespace. Backend-managed IDs grow upward from the low end;
client-local pane IDs grow downward from the high end. Actors therefore use one
pane ID shape without confusing a local pane with a managed Wayland pane.

## Fallback And Failure

The client immediately uses deterministic native flat layout when no root
layout actor is configured or the accepted lease tree no longer matches current
inventory. It also falls back after an invalid root candidate. A later valid
actor outbox message can become authoritative again.

Failure of a wrapper collapses that node and promotes its `rest` child rather
than revoking the useful inner layout. Removed plugin-owned panes follow normal
pane lifecycle independently. Reattachment receives a new lease generation, so
late results from the failed wrapper cannot modify the promoted subtree.

Actor failure follows normal generation failure and replacement rules. It does
not introduce a layout-specific failure channel or block the Winit event loop.

Showing, hiding, and focusing compound plugin UI is a layout-tree action. The
layout actor retains or restores its subtree, resolves its focus target, and
emits a new leased fragment. The transitional client-pane toggle message must
not grow into a general collection of switcher-specific host commands.

Related:

- [[presentation-client]]
- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[plugin-ui]]
- [[../concepts/presentation-surface]]
