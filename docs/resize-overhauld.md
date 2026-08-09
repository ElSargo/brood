---
status: target
type: plan
authority: product
last-reviewed: 2026-08-09
---

# Resize And Synchronized Paint Overhaul

## 1. Purpose

This document specifies the target resize and pane-border system.

The system lets each layout actor select its resize behavior.
The system also lets a layout actor request synchronized paint for selected panes.
The presentation client controls input routing, validation, staging, and final presentation.

This design keeps the recursive layout-lease system.
It does not use a synchronous layout provider.
The first implementation keeps one delegated child named `rest` for each lease.

## 2. Goals

The design has these goals:

- Draw one clear border between adjacent panes.
- Move borders and pane content in the same presented frame.
- Let layout actors define pointer interception zones.
- Let a pointer policy consume, pass through, or modify a pointer event.
- Keep layout changes in the normal actor mailbox path.
- Let an actor name the panes that must paint before presentation.
- Notify independent panes at the same time.
- Keep visible and staged content separate.
- Support recursive layout leases without a global layout actor.
- Keep resize work bounded during continuous pointer motion.
- Support plugin panes and managed Wayland panes.

## 3. Non-goals

This design does not move durable layout state into the host.
It does not let a policy provider submit a layout.
It does not let a policy provider start a paint transaction.
It does not require all visible panes to paint for each resize.
It does not make each internal split a layout lease.
It does not add multiple child slots to a layout lease.
It does not send placement rectangles to the backend.

## 4. Terms

### 4.1 Layout lease

A layout lease gives one actor authority over one layout region.
The lease has an identity and a generation.

### 4.2 Interaction zone

An interaction zone is a bounded input region in a layout fragment.
The zone uses coordinates that are local to its layout lease.

### 4.3 Interaction ID

An `InteractionId` identifies one pointer interaction.
The host creates the ID after a policy provider acquires pointer capture.
The ID stays stable until capture ends or fails.

### 4.4 Resize request

A resize request is an actor command that proposes new geometry.
The request also names the panes that block presentation.

### 4.5 Resize epoch

A `ResizeEpoch` identifies one accepted target geometry.
The host creates this value after it validates a resize request.

### 4.6 Affected pane

An affected pane has a new allocation in the target geometry.
The host derives this set from the accepted layout.

### 4.7 Wait pane

A wait pane must supply content for a resize epoch before atomic presentation.
The requesting actor explicitly names each wait pane.

### 4.8 Visible state

Visible state is the geometry and content in the last presented frame.

### 4.9 Staged state

Staged state is accepted geometry and content that is not yet visible.

## 5. Design Rules

The system must obey these rules:

1. The policy provider makes only an immediate input-routing decision.
2. The policy provider can send bounded actor messages.
3. The layout actor decides if an input event changes layout.
4. The layout actor starts each synchronized update.
5. The actor must explicitly name each wait pane.
6. The host must notify affected panes without unnecessary serial waits.
7. A draw can satisfy only the resize epoch that it names.
8. Partial staged content must not replace visible content.
9. The host must bound all wait operations.
10. The host must validate all lease, pane, revision, and epoch values.
11. Rendering and hit testing must use the same presented geometry.
12. A policy callback must not wait for actor work or paint work.

## 6. Ownership

### 6.1 Presentation client

The presentation client owns these functions:

- Compose the recursive lease tree.
- Validate layout fragments and interaction zones.
- Select the top input target.
- Call the applicable pointer policy provider.
- Maintain pointer capture.
- Create resize epochs.
- Send resize notifications.
- Keep visible and staged content.
- Track pane readiness.
- Draw native borders and other native chrome.
- Present an accepted staged region.
- Apply time limits and failure behavior.

### 6.2 Pointer policy provider

The pointer policy provider owns these decisions:

- Consume the event.
- Pass the event through.
- Return an approved event modification.
- Acquire, keep, or release pointer capture.
- Send bounded messages to actors.

The provider does not own layout state.
The provider does not submit a layout fragment.
The provider does not create a resize epoch.
The provider does not wait for paint completion.

### 6.3 Layout actor

The layout actor owns these functions:

- Keep its durable layout model.
- Define interaction zones in its layout fragment.
- Process pointer messages from a policy provider.
- Coalesce obsolete pointer messages.
- Calculate new geometry.
- Select the synchronized-paint mode.
- Name the panes that block presentation.
- Submit the synchronized layout request.
- Process completion, failure, and supersession messages.

### 6.4 Pane actor

A pane actor owns its pane content.
It processes a resize notification.
It submits a tagged draw or a reuse response.

### 6.5 Backend

The backend owns managed Wayland window lifecycle and buffer production.
It does not own presentation placement.
It correlates a resize epoch with the applicable Wayland configure operation.

## 7. Layout Fragment Additions

### 7.1 Interaction zones

A layout fragment can contain zero or more interaction zones.

The target data has this shape:

```text
InteractionZone {
    id: LayoutItemId,
    transform: LocalTransform,
    z: f64,
    event_mask: PointerEventMask,
    cursor: CursorRole,
    input: ZoneInputBehavior,
}
```

The host combines the lease identity with the local zone identity.
This combination gives the zone a stable composed identity.

```text
ComposedZoneId {
    lease: LayoutLeaseId,
    lease_generation: u64,
    local_item: LayoutItemId,
}
```

The host transforms and clips each zone during recursive composition.
A zone cannot receive input outside its lease viewport.

### 7.2 Zone event mask

The event mask can select these pointer event classes:

- Enter and leave.
- Motion.
- Button press and release.
- Axis or wheel input.
- Cancel.

The first implementation must support button, motion, release, and cancel.
The host can add the other event classes later.

### 7.3 Zone input behavior

The initial zone behaviors are `Hit` and `Drag`.
A `Hit` zone receives applicable events while the pointer is in the zone.
A `Drag` zone can acquire pointer capture.

### 7.4 Divider chrome

A layout actor can submit a native divider item.
The divider item refers to one interaction zone.

The actor owns divider geometry.
The presentation client owns divider appearance.

The visible divider and its input zone can have different widths.
The client must draw one shared line between adjacent panes.
The client must not draw two overlapping pane outlines for one divider.

The client must snap a thin divider to device pixels.
The client can use a larger invisible zone for pointer input.

## 8. Pointer Policy

### 8.1 Policy query

The host calls at most one pointer policy provider for one event.
The host selects the top zone from the composed hit stack.

The query has this target shape:

```text
PointerPolicyQuery {
    interaction: Option<InteractionId>,
    event_sequence: u64,
    lease: LayoutLeaseToken,
    zone: LayoutItemId,
    phase: PointerPhase,
    local_position: LocalPoint,
    button: Option<u32>,
    modifiers: u32,
    captured: bool,
}
```

The position is local to the lease viewport.
The query also identifies the local interaction zone.

### 8.2 Policy result

The result has this target shape:

```text
PointerPolicyResult {
    delivery: PassThrough | Consume | Modify,
    capture: Unchanged | Acquire | Release,
    modification: Option<PointerModification>,
    messages: Vec<ActorMessage>,
}
```

The host validates all modifications.
The host applies the delivery decision before it sends actor messages.
The host creates the interaction ID before it sends the first captured-event message.
The host then puts the actor messages in normal mailboxes.

### 8.3 Capture

A consumed press can start pointer capture.
Captured motion and release events go to the same policy provider.
The events continue to use the same zone and interaction ID.

The host releases capture after one of these conditions:

- The policy provider requests release.
- The final captured button is released.
- The lease becomes invalid.
- The policy provider fails.
- The window loses the applicable input scope.

The host sends a cancel message when capture ends because of failure.

### 8.4 Hover

The host must not call WASM only to select a resize cursor.
The zone supplies a cursor role in its accepted layout fragment.
The host can draw hover and active divider states.

## 9. Actor Pointer Messages

A policy provider can address a pointer message to its layout actor.
The provider supplies the target actor and bounded payload.
The host adds trusted pointer context to the message.
The message then uses the normal bounded mailbox path.

The message must include these values:

- Optional interaction ID.
- Event sequence.
- Lease token.
- Zone identity.
- Pointer phase.
- Lease-local position.
- Applicable button and modifier state.

The actor must process event sequences in order.
The actor can discard obsolete motion messages.
The actor must keep the final release or cancel message.

A captured-event message always has an interaction ID.
An uncaptured event message can have no interaction ID.

The actor can choose not to change layout.
This choice does not change the policy decision for the original event.

## 10. Synchronized Layout Request

### 10.1 Request shape

The layout actor starts synchronization with one host command.

```text
SubmitSynchronizedLayout {
    request_id: u64,
    interaction: Option<InteractionId>,
    event_sequence: Option<u64>,
    lease: LayoutLeaseToken,
    inventory_revision: u64,
    base_layout_revision: u64,
    fragment: LayoutFragment,
    wait_for_panes: Vec<PaneId>,
    presentation: PresentationMode,
}
```

The command contains the target fragment and the wait set.
This single command prevents an ordering race between geometry and synchronization.

The actor can use this command without a pointer interaction.
For example, a keyboard action can request the same synchronized behavior.

### 10.2 Wait set

The wait set is explicit and fixed when the host accepts the request.
The host must reject duplicate pane identities.

Each wait pane must meet these conditions:

- The pane exists in the current client catalog.
- The pane is in the requesting lease subtree.
- The pane is eligible for presentation in the target layout.
- The pane identity is valid for the current inventory revision.

A parent actor can wait for a pane in its delegated child subtree.
This permission does not give placement or input authority for that pane.

The lease state must publish a read-only `waitable_panes` list.
The list contains eligible pane identities in the lease subtree.

### 10.3 Affected panes

The host derives the affected pane set from visible and target allocations.
An affected pane can be outside the explicit wait set.

The host notifies all affected panes.
Only panes in the wait set block atomic presentation.

The actor can include an unchanged pane in the wait set.
The host then requests a synchronized repaint for that pane.

### 10.4 Presentation modes

The first implementation supports these modes:

- `Independent` presents geometry without a paint barrier.
- `HoldThenAtomic` keeps the old region until the wait set is ready.
- `ScaleThenAtomic` shows scaled visible content until the wait set is ready.

`ScaleThenAtomic` is the default mode for interactive resize.

The host sets the maximum wait time.
An actor cannot request an unlimited wait.

## 11. Resize Epoch

The host creates a resize epoch after successful validation.

```text
ResizeEpoch {
    id: u64,
    lease: LayoutLeaseId,
    lease_generation: u64,
    base_layout_revision: u64,
    target_layout_revision: u64,
}
```

The epoch is unique in one presentation client generation.
The host sends the epoch to the requesting actor.
The host also sends it in each applicable resize notification.

The policy provider does not receive authority from the epoch.

## 12. Recursive Resize Processing

### 12.1 Lease region resize

A changed `rest` allocation changes the viewport of its child lease.
The host sends a region-resize message to the child layout actor.

```text
LayoutRegionResized {
    resize_epoch: ResizeEpoch,
    lease: LayoutLeaseToken,
    size: LayoutSize,
    scale: f32,
}
```

The child actor selects one of these responses:

- Submit a new fragment for the resize epoch.
- Reuse its current fragment for the resize epoch.
- Report a failure for the resize epoch.

The child response must name the resize epoch.
The host rejects a response for an obsolete epoch.

### 12.2 Parallel branch processing

The host must send independent region and pane messages without a serial wait.
The host must not wait for the complete child region before it notifies directly owned panes.

When one child fragment arrives, the host can calculate allocations in that branch.
The host then sends pane-resize messages for that branch.
Other independent pane work continues at the same time.

### 12.3 Temporary projection

The host can project the last accepted child fragment into a new `rest` allocation.
This projection supplies the temporary image for `ScaleThenAtomic` mode.

The temporary projection does not become durable actor state.
The child actor can submit different final geometry for the resize epoch.

## 13. Pane Resize And Paint

### 13.1 Plugin pane notification

The host sends a tagged resize or repaint notification to each applicable plugin pane.

```text
PaneResize {
    resize_epoch: ResizeEpoch,
    pane: PaneId,
    logical_size: LayoutSize,
    scale: f32,
}
```

The host can coalesce consecutive notifications for the same pane.
The host must keep the newest epoch notification.

### 13.2 Plugin draw

A plugin draw packet can contain an optional resize epoch.

```text
SubmitDraw {
    pane: PaneId,
    sequence: u64,
    resize_epoch: Option<ResizeEpochId>,
    packet: DrawPacket,
}
```

A tagged draw becomes staged content for its epoch.
It does not immediately replace visible content.

### 13.3 Content reuse

A pane can reuse its current content.

```text
ReusePaneContent {
    pane: PaneId,
    resize_epoch: ResizeEpochId,
}
```

The host validates that reusable content exists.
The host then marks the pane ready for the epoch.

### 13.4 Managed Wayland pane

The presentation client sends the resize epoch with the managed resize request.
The backend associates the epoch with the applicable configure serial.

The backend marks the pane ready after it gets a suitable committed buffer.
The backend sends the frame and resize epoch to the presentation client.

The correlation rule must not treat an old buffer as new epoch content.

## 14. Staged Presentation

### 14.1 State separation

The renderer keeps one visible state and bounded staged state.

```text
VisibleState {
    layout_revision,
    geometry,
    pane_content,
}

StagedResize {
    resize_epoch,
    geometry,
    wait_set,
    ready_set,
    staged_content,
    deadline,
}
```

The renderer must not overwrite visible pane content with partial staged content.

### 14.2 Readiness

A wait pane is ready after one of these events:

- The host accepts a matching tagged draw.
- The host accepts a matching reuse response.
- The managed pane supplies a matching buffer.
- The host replaces a failed pane with accepted recovery content.

The staged region is ready when all wait panes are ready.

### 14.3 Atomic presentation

The host presents staged geometry and staged wait-pane content in one frame.
The host also presents the applicable border geometry in that frame.

Unwaited panes can use their newest available content.
They do not delay the atomic presentation.

After presentation, the staged state becomes visible state.
The host sends a completion message to the requesting actor.

### 14.4 Region readiness

A completed synchronized child region can satisfy a dependency in its parent region.
The host calculates this state from the explicit pane wait sets.

The host must not use actor response order as presentation order.

## 15. Concurrency And Supersession

### 15.1 Parallel work

The host must use these parallel operations where possible:

- Send independent layout-region messages without serial waits.
- Send pane-resize messages without serial waits.
- Send managed resize requests on independent backend lanes.
- Compile independent plugin draws when renderer resources permit it.
- Upload staged resources as they arrive.

The completion time must depend on the slowest required branch.
It must not depend on the sum of independent branch times.

### 15.2 Continuous pointer motion

One pointer interaction can produce many actor messages.
The actor can submit fewer resize requests than the number of pointer events.

The host keeps at most one active staged resize for one conflicting region.
A newer request from the same interaction supersedes the older request.

The host discards obsolete readiness state.
The host keeps visible content until a newer request is ready.

The final release message must not be coalesced away.
The final resize request must converge or report a failure.

### 15.3 Disjoint transactions

The host can process transactions for disjoint regions at the same time.
Two transactions conflict if they change the same lease region or pane.

The host must not run conflicting staged geometry at the same time.
The host can supersede, reject, or queue a conflicting request.
The result must be deterministic.

The first implementation must supersede requests from the same interaction.
It must reject other conflicting requests with a `Busy` result.

## 16. Native Window Resize

A native window resize does not use a pointer policy provider.
The host must resize the presentation surface immediately.
The host must show a projected visible layout during continuous native resize.

After a settlement interval, the host sends a region-resize message to the root layout actor.
The root actor can submit a synchronized layout request.
The request uses the same resize-epoch and wait-set mechanism.

This path replaces the separate durable native-resize reconciliation path.
It keeps immediate surface feedback and actor-owned final layout behavior.

## 17. Borders And Painting Order

The client renders a frame in this order:

1. Clear the presentation target.
2. Draw pane content in composed paint order.
3. Draw native dividers and pane chrome.
4. Draw overlays that have a higher z value than the dividers.

The renderer gets pane rectangles, dividers, and hit zones from one presented layout.
The renderer must not calculate border positions from a different layout revision.

The focused, hovered, and captured states can change divider color.
These state changes do not require a layout actor draw.

The fallback layout must generate divider items and interaction zones.
The default workspace actor must also generate them.

## 18. Validation

The host must reject a synchronized layout request for these conditions:

- The lease token is stale.
- The inventory revision is stale.
- The base layout revision is not applicable.
- The fragment references an unauthorized pane.
- The fragment has an invalid `rest` slot.
- An interaction zone is outside its lease bounds.
- A zone identity is zero or duplicated.
- A wait pane is duplicated.
- A wait pane is outside the lease subtree.
- A wait pane is not eligible for the target layout.
- The request exceeds a configured item or byte limit.
- The request conflicts with an active transaction.

The host must report the rejection to the actor.
The host must keep the current visible layout after rejection.

## 19. Failure And Time Limits

No synchronized update can wait without a time limit.
The host owns the maximum time limit.

The system uses these failure rules:

- A removed wait pane becomes accepted recovery content or an unavailable result.
- A failed plugin pane can use native recovery chrome.
- A disconnected managed pane can use its current content or a placeholder.
- A failed child layout actor can use its last accepted fragment.
- A stale tagged draw cannot satisfy the current epoch.
- A policy-provider failure releases pointer capture.

`ScaleThenAtomic` uses projected visible content until success or timeout.
After timeout, the host presents degraded content or cancels the transaction.
The selected fallback must be part of the presentation-mode contract.

The first implementation uses degraded presentation after timeout.
It must mark each missing pane in diagnostics.

## 20. Diagnostics

Diagnostics must include these values:

- Interaction ID.
- Pointer event sequence.
- Lease identity and generation.
- Actor request ID.
- Resize epoch.
- Base and target layout revisions.
- Affected pane set.
- Wait pane set.
- Ready pane set.
- Superseded epoch count.
- Time from request acceptance to pane notification.
- Time from notification to each pane result.
- Time from request acceptance to presentation.
- Timeout and degraded-pane information.

The host must report why it rejects a request.
The host must report why it releases pointer capture.

## 21. Implementation Plan

### Phase 1: Portable layout model

Extend `brood-layout` with interaction zones and composed zone identities.
Add validation, recursive transforms, clipping, and zone hit testing.
Add divider chrome roles.

Add tests for these conditions:

- Zone transforms through nested `rest` slots.
- Zone clipping at each lease boundary.
- Stable composed identities.
- Correct z-order selection.
- Rejection of invalid zone and divider data.

Primary code area:

- `crates/brood-layout/src/lib.rs`

### Phase 2: Portable WASM types

Add the new wire types to `brood-wasm`.
Add types for pointer policy queries and results.
Add types for synchronized layout requests and resize epochs.
Add optional resize-epoch fields to pane resize and draw messages.
Move common host messages out of the global-key-policy module.

Primary code areas:

- `crates/brood-wasm/src/policy_provider.rs`
- `crates/brood-wasm/src/resolved_layout.rs`
- New pointer-policy and common host-message modules.
- `crates/brood-wasm/wit/brood-plugin.wit`
- `crates/brood-wasm/witx/brood_preview1.witx`

### Phase 3: Pointer policy lane

Add a pointer policy-provider kind.
Associate the provider with a layout lease holder.
Build one bounded query for each selected zone event.
Validate the result before input routing continues.

Implement pointer capture with interaction IDs and event sequences.
Send provider messages through normal actor mailboxes.
Do not add layout submission to the policy lane.

Primary code areas:

- `crates/brood-wasm/src/policy_provider.rs`
- `crates/brood-wasm/src/policy_provider_link.rs`
- `clients/brood-client/src/main.rs`
- A new client pointer-policy module.

### Phase 4: Plugin-only vertical slice

Implement one synchronized transaction for two plugin panes.
Keep visible plugin textures separate from staged textures.
Let one pane delay its tagged draw.
Confirm that the host does not show a mixed resize frame.

Implement explicit wait sets, reuse responses, completion, and timeout.
Implement parallel pane-resize notification.

This phase does not require managed Wayland correlation.

### Phase 5: Recursive layout propagation

Add tagged layout-region resize messages.
Let a child actor submit or reuse a fragment for an epoch.
Process independent child branches at the same time.

Add `waitable_panes` to lease-scoped layout state.
Keep wait authority separate from placement authority.

Test a parent divider that changes a child `rest` allocation.
Test direct parent panes and child panes with different response times.

### Phase 6: Transaction coordinator

Add a client-owned resize transaction coordinator.
The coordinator keeps visible state, staged state, and readiness state.

Implement these functions:

- Epoch creation.
- Request validation.
- Affected-pane calculation.
- Wait-set tracking.
- Supersession.
- Disjoint transaction processing.
- Deadline processing.
- Atomic promotion to visible state.

Move current native-window settlement behavior into this coordinator.

Primary code area:

- `clients/brood-client/src/main.rs`
- New focused modules under `clients/brood-client/src/`.

### Phase 7: Managed Wayland panes

Carry resize epochs with managed resize requests.
Associate each epoch with its Wayland configure serial.
Return the epoch with the first suitable committed frame.

Keep normal untagged frame delivery available.
Do not let an old frame satisfy a new resize epoch.

Primary code areas:

- Presentation transport types.
- `clients/brood-client/src/main.rs`.
- Managed resize handling in `crates/broodd`.
- Fast-path frame and input records.

### Phase 8: Native border renderer

Add a native chrome pipeline to the presentation renderer.
Render divider items after pane content.
Use the same presented geometry for dividers and zone hit testing.

Add hover, focus, and capture states.
Keep the visible line separate from its larger input zone.

Primary code areas:

- `clients/brood-client/src/main.rs`
- New native-chrome renderer module.

### Phase 9: Default layout actor

Update the default workspace actor to emit dividers and interaction zones.
Add its pointer policy provider.
Send pointer messages from the provider to the workspace actor.

Let the actor calculate and store split ratios.
Let the actor submit synchronized layout requests with explicit wait sets.

Primary code areas:

- `plugins/default-workspace/src/lib.rs`
- `plugins/default-workspace/src/backend_layout.rs`
- Shared guest-side policy and layout helpers.

### Phase 10: Native window resize migration

Keep immediate WGPU surface reconfiguration.
Use projected visible content during continuous native resize.

After settlement, notify the root layout actor.
Use the common synchronized-layout transaction for final content.

Remove the old duplicate reconciliation path after equivalent proof passes.

### Phase 11: Documentation and cleanup

Update the target system documents after the vertical slice works.
Remove statements that say layout has no synchronous interaction policy.
Keep the rule that policy providers cannot submit layout.

Update these documents:

- `docs/systems/layout-actor.md`
- `docs/systems/input-shortcuts.md`
- `docs/systems/plugin-ui.md`
- `docs/systems/presentation-client.md`
- `docs/systems/presentation-transport.md`
- `docs/reference/wasm-fast-lanes.md`

## 22. Verification Plan

Use the checked-in Nix development environment for all build and test work.
Use the built-in nested test path for automated behavior tests.

### 22.1 Portable tests

Test zone validation, transforms, clipping, and hit order.
Test wait-set validation and stable pane identity.
Test transaction state changes and supersession.

### 22.2 Policy tests

Prove that a pointer policy can consume and capture a press.
Prove that it can pass an event to the pane.
Prove that it can send an actor message.
Prove that it cannot submit a layout or create an epoch.

### 22.3 Atomic-paint test

Use two plugin panes in one synchronized request.
Delay one pane after both panes receive resize notifications.

Capture the visible frame before the second pane becomes ready.
The frame must contain only the old complete state or the projected old state.

Capture the first frame after readiness.
The frame must contain the new geometry, new borders, and both tagged draws.

### 22.4 Explicit wait-set test

Resize three panes and wait for only two panes.
Delay the third pane.
The first two panes must control barrier completion.
The third pane must not delay presentation.

### 22.5 Parallel-work test

Use multiple actors with controlled response delays.
Record the notification and completion times.

The host must start independent work without serial waits.
The total wait must track the slowest required branch.

### 22.6 Recursive-layout test

Use a parent wrapper, one `rest` child, and directly owned parent panes.
Resize the parent `rest` allocation with a captured divider.

The temporary frame must project the child region.
The final frame must wait only for the named panes.
The parent border and child content must change in one presented frame.

### 22.7 Continuous-drag test

Send many real pointer-motion events during one capture.
Confirm that staged transaction count stays bounded.
Confirm that the final release position becomes durable actor state.

### 22.8 Managed-pane test

Use real managed Wayland clients.
Send concurrent resize requests with one resize epoch.

Confirm configure-serial and frame correlation.
Confirm that an old buffer cannot satisfy the wait set.

### 22.9 Failure tests

Test actor failure, pane removal, backend disconnect, and timeout.
Confirm that no test can leave a permanent paint barrier.
Confirm that diagnostics identify every degraded pane.

### 22.10 Visual proof

Use real pointer input, screenshots, and transaction metrics.
Verify thin shared borders at normal and fractional scale factors.
Verify hover, capture, continuous drag, final sharp paint, and timeout fallback.

## 23. Acceptance Criteria

The overhaul is complete when all these statements are true:

- A layout actor can define a lease-local pointer interception zone.
- The pointer policy only controls immediate event routing and actor messages.
- The layout actor can submit a synchronized update after it gets a message.
- The actor explicitly names all panes that block the update.
- The host notifies independent panes without unnecessary serial waits.
- Plugin draws can name the accepted resize epoch.
- Managed frames can be correlated with the accepted resize epoch.
- The host never presents partial staged content for a waited pane set.
- Recursive child regions can update in parallel.
- A parent can wait for eligible panes in its child subtree.
- Borders and pane content use the same presented geometry.
- Continuous pointer motion keeps bounded staged work.
- A failed pane or actor cannot keep the renderer blocked.
- Native window resize uses the same final synchronization mechanism.
- Nested tests show the correct visible behavior with real input and screenshots.

## 24. Initial Delivery Boundary

The first complete delivery uses this limited scope:

- One root workspace layout actor.
- One pointer policy provider for that actor.
- Divider drag for plugin panes.
- Explicit wait sets.
- `ScaleThenAtomic` presentation.
- One active transaction in the workspace region.
- Tagged plugin draws and reuse responses.
- Native divider rendering.

This delivery proves the complete ownership and synchronization loop.
Later deliveries add recursive child actors, managed panes, and native window resize.
