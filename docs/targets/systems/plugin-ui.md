---
status: target
type: system
authority: product
last-reviewed: 2026-09-02
---

# Plugin UI And Pane Rendering

The selected ordinary plugin UI path is the host-managed semantic document
model in [[gpui-presentation]]. Plugin application state remains actor-owned,
while the GPUI adapter validates, retains, patches, lays out, and presents the
component tree. The canvas packet contract in [[plugin-graphics]] and its
host-driven [[plugin-pure-renderer]] remain supported for custom content and
the terminal canvas.

## Ownership

The presentation client owns:

- pane shells, tabs, borders, title bars, menus, overlays, and recovery UI;
- pane placement, resize, focus, visibility, scale, and lifecycle events;
- delivery of pane-local keyboard, pointer, touch, text, and IME input;
- validation, reconciliation, transient state, hit testing, and rendering of
  managed semantic documents;
- validation and rendering of canvas draw packets;
- native text shaping and platform font integration;
- final clipping, composition, scheduling, and renderer recovery;
- projection of optional accessibility metadata onto platform APIs.

The plugin actor and its SDK own:

- pane-local application and UI state;
- stable semantic node identity and document revisions;
- domain values, commands, and durable editor/document state;
- semantic theme tokens and bounded style overrides;
- packet encoding and submission;
- accessibility labels, roles, values, relationships, and commands.

If an actor exits, the pane identity can remain host-owned. The client replaces
plugin content with native recovery chrome such as restart, close, and logs
actions without needing to understand the failed UI's components.

## SDK Components

SDK component helpers construct stable protocol nodes such as:

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

Their common output is a bounded semantic snapshot or atomic patch. The host
maps that protocol to GPUI and returns semantic actions. SDK APIs may be
immediate, retained, or reactive, but GPUI, GTK, browser DOM types, arbitrary
host closures, and toolkit widget handles never become the plugin ABI. Canvas
helpers remain separate and produce canvas resources for explicit canvas or
terminal-view nodes.

## Pane Lifecycle

A presentation actor may request or be assigned a pane. The client creates the
pane shell and routes lifecycle and pane-local input events to the actor:

```text
Client -> Actor: PaneCreated { pane, size, scale, capabilities }
Client -> Actor: PaneResized { optional resize epoch } / PaneFocused / PaneHidden / PaneDestroyed
Client -> Actor: Pointer / Key / Text / Ime
Actor -> Client: PublishManagedUi { snapshot | atomic patch | remove pane }
Client -> Actor: UiAction { document and interaction revisions }
Client -> Actor: RenderHint { token, target time, deadline, relevant panes, watermark }
Actor -> Client: RequestRender / SetRenderSubscription
Actor -> Client: PublishRenderState / RequestRender
Actor -> Client: FinishActorFrame { token, changed pane draw packets } (asynchronous path)
Actor -> Client: ReusePaneContent { pane, resize epoch }
Actor -> Client: CommandRequest / Status
```

The actor returns from each dispatch without waiting for I/O, a worker, or GPU
execution. It may delegate unrelated work and request rendering when a
completion arrives. On the pure path, the host invokes a reserved render phase
in the same actor instance with its live state and exact current geometry. On
the asynchronous path, the render hint is actor-scoped and the actor atomically
finishes all changed owned panes in one token-bearing batch. The complete pacing
and token lifecycle is defined by [[render-scheduling]].

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
owns pane and managed-node focus. GPUI owns managed hover, press, scrolling,
selection, composition, pointer capture, and other transient interaction state;
the actor receives semantic actions. Canvas nodes continue to receive
normalized pane-local input for plugin-defined hit testing.

Keyboard events use stable semantic names for navigation, editing, and function
keys. Modifier bits are Ctrl=1, Alt=2, Shift=4, and Super=8 across key and
pointer events. Printable insertion uses text events; a plugin must not derive
locale-dependent text from the physical scancode.

The client implements native text input, IME, clipboard, selection, carets, and
navigation for managed fields according to their controlled, host-transient, or
hybrid state mode. Durable document state remains actor-owned. Canvas editors
may still combine pane events with native text services from
[[plugin-graphics]].

## Native Chrome

Native pane chrome remains available by default so plugins do not redraw
workspace borders, tabs, drag handles, recovery controls, or presentation menus.
The actor may request chrome roles and actions, but the client decides their
placement, style, focus behavior, and composition.

Client chrome is outside the plugin component SDK and cannot be overwritten by
a full-pane draw packet without a separate presentation role.

## Accessibility

Managed nodes carry stable accessibility roles, labels, values, relationships,
states, and commands in the same revisioned document used for rendering. The
client validates and projects this tree to the platform adapter. Canvas nodes
may publish a bounded companion semantic subtree when pixels alone cannot
express their meaning.

## Example

A session-switcher actor owns its query and fuzzy-matched session model and
publishes search-field, virtual-list, selectable-row, and button nodes. GPUI
handles editing and interaction locally; a selection produces a revisioned
semantic action that the actor reduces into a normal session command and later
document patch.

## Advanced Drawing

The base UI path uses managed semantic documents. Applications that need custom
geometry or GPU-style resources use canvas nodes and, where necessary, the
separate
[[plugin-advanced-graphics]] protocol. Advanced graphics is an escape hatch, not
the substrate that every SDK component must encode against.

## Validation And Diagnostics

The client validates document/patch bounds, node identity and depth, semantic
relationships, resource ownership, pane authority, input revisions, and canvas
packets. Diagnostics report rejected, stale, or superseded publications and
actions, actor latency, frame/update counters, text failures, resource use, and
recovery state.

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
