---
status: target
type: system
authority: product
last-reviewed: 2026-07-13
---

# Message-Based Layout

The target layout model gives plugins flexible policy control while keeping presentation execution client-owned. Layout normally runs in a presentation actor next to the viewport, so pane changes and user layout actions do not cross the backend connection. Layout is not a synchronous policy-provider lane. It is an actor-owned local convergence path:

```text
presentation-client layout event
    -> presentation actor mailbox
    -> actor updates client-local layout state
    -> actor sends SetResolvedLayout to the local client host
    -> local validation
    -> accepted viewport layout
    -> WGPU render, hit testing, focus, and input delivery
```

## Ownership

The presentation client owns:

- presentation surfaces and native chrome
- final layout validation and canonicalization
- pane bounds, clipping, hit testing, and focus routing
- pointer capture and drag bookkeeping
- fallback layout when plugin layout is missing or invalid

Plugins own:

- layout policy state and preferences
- split/tab/stack/floating/dock models internal to the plugin
- rules for placement, resizing, focus preference, and visibility
- actor logic that turns client events into candidate resolved layouts

The plugin's internal layout tree is not the client host ABI. The client accepts only a bounded resolved layout through a locally validated message. Persistence is explicit: layout state remains client-local unless the actor chooses to coordinate it with a session actor or backend storage.

## Layout Events

The presentation client emits normal actor messages when layout-relevant state changes. Examples include:

```text
PaneMapped
PaneUnmapped
ActiveSessionChanged
FocusChanged
OutputResized
LayoutStateSnapshot
```

The current scaffold emits a `layout-state` snapshot containing visible panes, focused pane, and presentation size. This should evolve toward typed event envelopes with revisions so stale actor responses can be rejected.

## SetResolvedLayout

Actors request a visual layout by sending a host message equivalent to:

```text
SetResolvedLayout {
  layout: ResolvedLayout,
  focus: optional<PaneId>,
}
```

The presentation client validates that pane items reference real panes in the current inventory and viewport, canonicalizes the result, rejects invalid or oversized layouts, and then commits it as the accepted viewport layout.

## Why Layout Is Not A Provider

The synchronous provider model remains valuable for global key handling because the presentation client must decide whether to consume or forward a key before it can continue.

Layout is different. It is usually an eventual-convergence problem: the world changes, then the visible layout should update promptly. If layout depends on plugin-owned split trees, session layout state, focus history, or dock state, a provider would still need a host-to-actor-to-publication round trip before it had current state. At that point the synchronous provider adds complexity without eliminating the real latency source.

## Fallback

The presentation client must always have a deterministic native fallback layout. Fallback is used when:

- no layout actor is registered
- the actor does not produce a layout yet
- the actor traps, stalls, or is disabled
- the candidate layout is invalid or too large

Fallback should keep panes visible enough for recovery and expose clear status so users can repair or disable a failing plugin.

Related:

- [[presentation-client]]
- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[plugin-ui]]
- [[../concepts/presentation-surface]]
