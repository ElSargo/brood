---
status: target
type: system
authority: product
last-reviewed: 2026-07-05
---

# Message-Based Layout

The target layout model gives plugins flexible policy control while keeping presentation execution native-owned. Layout is not a synchronous policy-provider lane. It is an actor-owned convergence path:

```text
host layout event
    -> plugin actor mailbox
    -> actor updates plugin layout state
    -> actor sends SetResolvedLayout host message
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
- fallback layout when plugin layout is missing or invalid

Plugins own:

- layout policy state and preferences
- split/tab/stack/floating/dock models internal to the plugin
- rules for placement, resizing, focus preference, and visibility
- actor logic that turns host events into candidate resolved layouts

The plugin's internal layout tree is not the host ABI. The host accepts only a bounded resolved layout through a host-validated message.

## Layout Events

The host emits normal actor messages when layout-relevant state changes. Examples include:

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

The host validates that pane items reference real active panes, canonicalizes the result, rejects invalid or oversized layouts, and then commits it as the accepted resolved layout.

## Why Layout Is Not A Provider

The synchronous presenter/consumer provider model remains valuable for global key handling because the host must decide whether to consume or forward a key before it can continue.

Layout is different. It is usually an eventual-convergence problem: the world changes, then the visible layout should update promptly. If layout depends on plugin-owned split trees, session layout state, focus history, or dock state, a provider would still need a host-to-actor-to-publication round trip before it had current state. At that point the synchronous provider adds complexity without eliminating the real latency source.

## Fallback

Manifold must always have a deterministic native fallback layout. Fallback is used when:

- no layout actor is registered
- the actor does not produce a layout yet
- the actor traps, stalls, or is disabled
- the candidate layout is invalid or too large

Fallback should keep panes visible enough for recovery and expose clear status so users can repair or disable a failing plugin.

Related:

- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[plugin-ui]]
- [[../concepts/presentation-surface]]
