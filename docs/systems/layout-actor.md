---
status: target
type: system
authority: product
last-reviewed: 2026-08-09
---

# Client-Local Layout Actor

Each presentation client owns a host-validated tree of layout leases. Every
lease is held by an ordinary mailbox-driven presentation actor. Layout does not
have a synchronous layout provider or a session-server implementation. A lease
holder may expose a synchronous pointer policy, but that policy can only route
the current event and enqueue bounded actor messages; it cannot submit layout
or start a paint transaction.

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
- pane bounds, interaction zones, divider chrome, clipping, z order, hit
  testing, and focus routing;
- pointer-policy invocation, interaction IDs, event sequencing, and capture;
- resize-epoch validation, staged pane content, deadlines, and atomic
  promotion;
- deterministic client-local fallback while no valid actor result is available;
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
The snapshot identifies inventory and presented-layout revisions separately and
publishes the eligible `waitable_panes` in the lease subtree.
Each snapshot includes the selected session, the lease's available managed
windows with their session and application identities, the backend/session
realm for every realm-bound managed or client-local pane, and `visible_panes`
as the client's default session-filtered subset. The available set defines
layout authority; `visible_panes` is a convenience policy input, not a
validation boundary. The default workspace keeps each backend in a disjoint
top-level column and arranges that backend's panes in an aspect-aware balanced
grid within its column, so panes from separate local or remote computers never
enter the same anonymous stack. Column width follows the amount of content in
the backend group. Incomplete final rows expand to use their available width
instead of leaving dead cells. This remains bounded by the actor inventory
limit rather than assuming a fixed pane count.

Other actors and the key lane may address ordinary commands such as
`layout-action` to the layout actor. An immediate candidate reaches the host as
`set-resolved-layout`; an update that needs a paint barrier uses one
synchronized-layout command containing its target fragment and explicit wait
set. Both leave the actor's ordinary host outbox. The host rejects direct layout
submission from every synchronous policy lane.

There is no synchronous request/return pairing requirement. Revisions make
state snapshots observable, while normal actor mailbox ordering determines
processing order. An actor may coalesce work or emit more than one update. The
host rejects stale lease generations and results computed from an obsolete
inventory revision.

A layout result contains the lease token, input revision, locally normalized
items, interaction zones, divider chrome, an optional `rest` child transform
and local paint position, and an optional focus request. The actor never
supplies its insertion point in the global tree; the lease determines where the
host inserts the fragment.

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

When a synchronized parent fragment changes its `rest` viewport, the host sends
`layout-region-resized` with the same resize epoch to the exact child lease
holder. The child must submit a new fragment, explicitly reuse its durable
fragment, or report failure for that epoch. A changed descendant `rest` repeats
the same process. The old child fragment can supply temporary projected content,
but it cannot become durable until the branch responds or the host records
bounded recovery at the transaction deadline.

The region size uses the same composition-pixel units as `layout-state`. The
message carries the presentation scale as separate metadata.

Region resolution is a geometry dependency. It is separate from the fixed,
explicit pane paint wait set. The host notifies directly owned panes while an
independent child region is still resolving. It notifies panes inside that child
branch only after the child response defines their final target bounds. Thus a
region can block final geometry without adding an unnamed pane to the paint
barrier, and only panes named by the accepted request can block paint readiness.

## Flat Resolved Layout

A resolved layout is a flat, client-validated presentation description. It may
contain pane items, overlay pane items, placeholders, handles, chrome, and
overlay metadata. Each item has a stable item ID, normalized transform, z
value, and input behavior.

An ordinary pane is base content. The client paints all ordinary panes before
native dividers and chrome, even when a pane has a higher z value. An explicit
overlay pane can paint after the intersecting dividers that have a lower z
value. If an intersecting divider has a higher z value, the overlay pane stays
in the base pane phase. Overlay metadata does not create visual content.

Pane items must reference an available pane identity in the current client
catalog and may appear at most once. Frame readiness is not part of layout
validation: an available managed pane without a mapped frame is rendered as a
placeholder in its specified rectangle and replaced in place when its frame
arrives. An actor may omit a pane to keep it out of this viewport. Non-pane
items let the client expose interactive targets without learning the actor's
internal model.

The portable `brood-layout` crate owns validation, normalized projection,
recursive lease-fragment composition, and hit testing. It does not depend on
Smithay, Winit, WGPU, transport code, or the WASM runtime.

## Pointer-Driven Synchronized Resize

Interaction zones are lease-local input geometry, separate from rendered pane
items. The client composes and clips them with the same transforms used for the
presented layout. It selects the top applicable zone, invokes that lease
holder's bounded pointer policy once, and keeps captured motion and the final
release on one interaction ID. Hover cursor selection and native divider state
remain client-local and do not invoke WASM.

The pointer policy may consume or pass the event and enqueue an ordinary actor
message. The layout actor owns the durable split ratio. If that message changes
geometry, the actor submits one synchronized layout command containing its
lease token, inventory and base-layout revisions, target fragment, presentation
mode, and explicit pane wait set.

The host issues each interaction ID and records each event sequence only after
normal actor delivery succeeds. The record is bound to the exact lease token
and holder actor generation. An interaction-bound synchronized request is valid
only when its interaction and event sequence match this host record. The host
keeps the final release or cancel sequence after capture ends, so the actor can
submit the final layout request. It removes the record only after the actor
mailbox accepts the terminal result for that request.

After validation, the client allocates a resize epoch, projects the last visible
content into the target geometry, and notifies all affected panes together.
Only a draw or reuse response tagged with that epoch can satisfy a waited pane.
Visible content and staged content remain separate until the complete wait set
is ready; geometry, pane content, and divider chrome are then promoted in one
presented frame. A newer request in the same interaction supersedes the staged
request, and every accepted request has a client-owned deadline and degraded
fallback.

## Pane Identity

The client presents managed panes and client-local WASM panes in one flat
identity namespace. Backend-managed IDs grow upward from the low end;
client-local pane IDs grow downward from the high end. Actors therefore use one
pane ID shape without confusing a local pane with a managed Wayland pane.

## Fallback And Failure

The client immediately uses a deterministic local fallback when no root layout
actor is configured or the accepted lease tree no longer matches current
inventory. The fallback uses the same aspect-aware balanced-grid policy,
without backend-specific grouping that requires actor state. It also falls back
after an invalid root candidate. A later valid actor outbox message can become
authoritative again.

## Native Window Resize

The presentation viewport is a normal resizable OS window with native
decorations and a minimum usable extent. Interactive resize is a client-local
visual transaction. Each native size event immediately reconfigures the WGPU
surface and reprojects the accepted normalized layout, so the existing managed
and plugin textures track the pointer without waiting on an actor or nested
client.

After native size events have been quiet for a short settlement interval, the
client publishes one coherent viewport revision to the root actor. The actor
may submit a final synchronized layout through the same resize-epoch and
wait-set path used by divider interaction. Plugin target recompilation and
managed Wayland size requests are tagged with that accepted epoch. New frame
and actor traffic may continue during the drag, but cannot bypass this barrier
and start a cascade of incompatible intermediate configurations.

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
