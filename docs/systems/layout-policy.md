---
status: target
type: system
authority: product
last-reviewed: 2026-07-03
---

# Layout Policy

The target layout model gives plugins flexible policy control while keeping presentation execution native-owned.

```text
pane inventory + presentation constraints + layout event + plugin state frame
    -> sync layout policy result
    -> host validation
    -> accepted resolved layout
    -> native render, hit testing, focus, and input delivery
```

## Ownership

The host owns:

- presentation surfaces and native chrome
- final layout validation and canonicalization
- pane bounds, clipping, hit testing, and focus routing
- pointer capture and drag bookkeeping
- fallback layout when plugin policy is missing or invalid
- persistence of user-approved layout preferences

Plugins own:

- policy state and preferences
- split/tab/stack/floating/dock models internal to the plugin
- rules for initial placement, resizing, focus preference, and visibility
- materialized latest state published through swapchains
- sync lane logic that returns candidate resolved layouts

The plugin's internal layout tree is not the host ABI. The host accepts only a bounded resolved layout or host-validated directives.

## Sync Layout Policy Lane

Layout is a sync policy lane because the host often needs an immediate answer when a pane maps, the presentation resizes, or a pointer drag affects visible bounds.

```text
LayoutQuery {
  reason,
  pane_inventory,
  constraints,
  current_focus,
  pointer_or_command_context,
}
+ latest plugin swapchain frame
    -> LayoutPolicyResult
```

Candidate result shape:

```text
LayoutPolicyResult {
  disposition: Abstain | CandidateLayout | Reject,
  layout: optional<ResolvedLayout>,
  focus: optional<PaneId>,
  directives: list<host-validated-directive>,
  messages: list<mailbox-message>,
}
```

The host validates the candidate before accepting it. Invalid candidates fall back to native policy and produce diagnostics.

## Resolved Layout

A resolved layout is a flat, host-validated presentation description. It is not the plugin's persistent state and not necessarily a split tree.

Resolved items can include:

- pane items
- empty regions and placeholders
- handles and drag targets
- native chrome items
- overlays and transient controls

Pane items reference real panes from the host inventory. Non-pane items use stable IDs so the host can report pointer, keyboard, or focus events back without understanding the plugin's internal policy model.

## Interactive Layout

Interactive behavior such as live split resizing should not require the sync lane to mutate private state during the host call.

The host query should include enough context to compute an immediate candidate:

```text
PointerDrag {
  item_id,
  drag_start,
  current_position,
  original_layout_snapshot,
}
```

The sync lane can return the current visual result from the latest state plus the drag delta. On commit or release it may also return a message asking the owning actor to persist the new ratio. The actor later publishes a new swapchain frame reflecting that durable state.

## Initial Pane Placement

Initial map policy follows the same pattern:

```text
PaneMapped pending pane
  -> host builds pane inventory and query
  -> sync layout policy returns candidate layout/focus or abstains
  -> host validates and commits
  -> actors observe committed events
```

This generalizes the current map-hook prototype without making a queued actor response part of the critical path.

## Fallback

Manifold must always have a deterministic native fallback layout. Fallback is used when:

- no layout policy lane is registered
- the lane abstains
- the lane traps, times out, or is disabled
- the candidate layout is invalid or too large
- required plugin state is missing or stale beyond host policy

Fallback should keep panes visible enough for recovery and should expose clear status so users can repair or disable a failing plugin.

## Relation To The Current WASM Map Hook

The current `wasm_map` lane asks a WASM module for a resolved layout when a pane maps. That is the prototype of the target sync layout policy lane.

The target changes the boundary from ad hoc JSON response plus host imports to a typed decision envelope backed by immutable swapchain state. The performance goal remains the same: no actor queue or message backlog is needed before the host can place a pane.

Related:

- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[plugin-ui]]
- [[../concepts/presentation-surface]]
- [[../reference/wasm-fast-lanes]]
