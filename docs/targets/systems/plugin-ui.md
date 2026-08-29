---
status: target
type: system
authority: product
last-reviewed: 2026-08-09
---

# Plugin UI And Pane Rendering

Plugin UI is actor-owned application state rendered through the canvas packet
contract in [[plugin-graphics]]. Rich components are SDK functions built from
that contract. The presentation client does not implement, validate, retain, or
patch a plugin component tree.

## Ownership

The presentation client owns:

- pane shells, tabs, borders, title bars, menus, overlays, and recovery UI;
- pane placement, resize, focus, visibility, scale, and lifecycle events;
- delivery of pane-local keyboard, pointer, touch, text, and IME input;
- validation and rendering of canvas draw packets;
- native text shaping and platform font integration;
- final clipping, composition, scheduling, and renderer recovery;
- projection of optional accessibility metadata onto platform APIs.

The plugin actor and its SDK own:

- pane-local application and UI state;
- components, layout, styling, themes, and widget identity;
- widget hit testing and event dispatch;
- text-editing behavior and document state;
- packet encoding and submission;
- optional semantic accessibility descriptions.

If an actor exits, the pane identity can remain host-owned. The client replaces
plugin content with native recovery chrome such as restart, close, and logs
actions without needing to understand the failed UI's components.

## SDK Components

Components are library code, not protocol entities. An SDK may expose functions
such as:

```text
column
list
button
checkbox
menu
text-input
editor
virtual-list
tooltip
```

Those functions may use immediate-mode, retained, reactive, or application-
specific guest-side internals. Their common output is canvas commands, image and
text-layout references, plus optionally accessibility metadata. SDK text helpers
call or cache the host `layout_text` service outside the packet and emit only
`draw-text-layout` commands. No component kind, property schema, patch stream,
native toolkit object, or widget event is part of the host ABI.

This keeps SDK evolution independent from the runtime. Different SDKs may offer
different component systems while producing compatible draw packets. Iced,
GPUI, GTK, browser DOM types, or another toolkit must not become the plugin ABI.

## Pane Lifecycle

A presentation actor may request or be assigned a pane. The client creates the
pane shell and routes lifecycle and pane-local input events to the actor:

```text
Client -> Actor: PaneCreated { pane, size, scale, capabilities }
Client -> Actor: PaneResized { optional resize epoch } / PaneFocused / PaneHidden / PaneDestroyed
Client -> Actor: Pointer / Key / Text / Ime
Client -> Actor: RenderHint { token, target time, deadline, relevant panes, watermark }
Actor -> Client: RequestRender / SetRenderSubscription
Actor -> Client: FinishActorFrame { token, changed pane draw packets }
Actor -> Client: ReusePaneContent { pane, resize epoch }
Actor -> Client: CommandRequest / Status
```

The actor returns from each dispatch without waiting for I/O, a worker, or GPU
execution. It may delegate unrelated work and request rendering when a
completion arrives. The render hint is actor-scoped, and the actor atomically
finishes all changed owned panes in one token-bearing batch. Drawing is normal
actor output, not a synchronous key decision lane. The complete pacing and
token lifecycle is defined by [[render-scheduling]].

A resize-epoch-tagged packet is staged rather than replacing the visible pane
immediately. When a synchronized layout waits for that pane, only a packet or
explicit content-reuse response carrying the accepted epoch marks it ready.
The client promotes the full waited set with its target geometry and native
chrome in one frame. Untagged packets remain the ordinary redraw path and never
satisfy a resize barrier accidentally.

The bundled terminal presentation actor follows this lifecycle without a
terminal-specific pane API. It converts pane size to rows and columns using the
native monospace cell metrics, sends coalescible resize messages to its session
actor, maps pane key and text events to terminal input bytes, and renders the
latest complete terminal revision with ordinary canvas commands and text
layout handles. The session actor, not the presentation actor, owns the shell.

## Input And Text Editing

The client converts platform events into a stable pane-local event model and
owns focus between panes and client chrome. Inside the pane, the SDK decides
which component is hovered, pressed, focused, scrolled, or edited.

Keyboard events use stable semantic names for navigation, editing, and function
keys. Modifier bits are Ctrl=1, Alt=2, Shift=4, and Super=8 across key and
pointer events. Printable insertion uses text events; a plugin must not derive
locale-dependent text from the physical scancode.

The client bridges platform text and IME input but does not own the plugin's
editor or text-field state. The SDK combines those events with native text
measurement and hit-testing services from [[plugin-graphics]] to implement
selection, carets, navigation, and drawing.

## Native Chrome

Native pane chrome remains available by default so plugins do not redraw
workspace borders, tabs, drag handles, recovery controls, or presentation menus.
The actor may request chrome roles and actions, but the client decides their
placement, style, focus behavior, and composition.

Client chrome is outside the plugin component SDK and cannot be overwritten by
a full-pane draw packet without a separate presentation role.

## Accessibility

Pixels and canvas commands do not preserve component semantics. An SDK may
therefore publish a companion accessibility description containing stable IDs,
roles, bounds, labels, values, states, and actions. The client validates and
projects that description to the platform accessibility adapter.

This semantic description is not a component tree or rendering input. It does
not prescribe SDK layout, styling, state ownership, or widget implementation.
Visual and semantic updates use explicit revisions so the client does not pair
incompatible geometry and accessibility metadata.

## Example

A session-switcher SDK may implement a list, buttons, query editor, selection,
and fuzzy matching entirely in guest code. The host provides session events,
command capabilities, pane input, canvas rendering, and text layout. A button
click becomes a pane-local pointer event interpreted by the SDK, followed by a
normal command request and render request.

The client never receives a `Button` node or preserves a host widget instance.

## Advanced Drawing

The base UI path uses canvas packets. Future applications that need custom
geometry, user shaders, or GPU-style resources may use the separate
[[plugin-advanced-graphics]] protocol. Advanced graphics is an escape hatch, not
the substrate that every SDK component must encode against.

## Validation And Diagnostics

The client validates packet bounds, resource ownership, pane authority, input
revisions, and optional semantic metadata. Diagnostics report rejected or
superseded UI updates, actor latency, frame/update counters, text failures,
resource use, and recovery state.

For input-driven panes, diagnostics correlate the first admitted key event with
the first subsequently refreshed plugin scene that is actually presented. This
key-to-present measurement covers the asynchronous actor, text-layout, packet,
and renderer path rather than stopping at mailbox admission.

SDK component correctness is tested at the SDK and workload level rather than by
adding component rules to the runtime. Real behavior tests should drive pointer,
keyboard, text, resize, focus, and scroll input through a nested presentation
client and verify both rendered output and client/actor metrics.

Related:

- [[plugin-graphics]]
- [[render-scheduling]]
- [[plugin-advanced-graphics]]
- [[presentation-client]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[terminal-plugin]]
- [[layout-actor]]
- [[../../concepts/pane]]
- [[../../experiments/session-switcher-ui]]
