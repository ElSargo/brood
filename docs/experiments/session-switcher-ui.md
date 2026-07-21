---
status: experiment
type: experiment
authority: product
last-reviewed: 2026-07-21
---

# Session Switcher UI Experiment

This experiment proves that a useful native-feeling plugin interface can be
built from guest-side SDK components which emit the base canvas packets in
[[../systems/plugin-graphics]]. It does not define or test a host-rendered
component tree.

## Goal

Build a session switcher whose application state, component behavior, layout,
styling, and pane-local hit testing live in a presentation actor and its SDK.
The presentation client supplies session events and commands, pane input, native
text services, canvas rendering, client chrome, focus between panes, and final
composition.

The experiment should answer:

- whether the canvas packet and native text service are sufficient for real
  Manifold shell UI;
- whether SDK component functions can remain concise without runtime component
  support;
- whether pane-local pointer, keyboard, text, scroll, focus, and resize events
  are sufficient for interactive widgets;
- which canvas, text, input, and SDK facilities are missing before the base ABI
  is frozen.

## User-Facing Workload

The experiment has two views owned by the same presentation-local application.

### Persistent Session Sidebar

The presentation surface has a persistent scrollable sidebar listing the
sessions known to the active workspace.

Expected behavior:

- the sidebar remains visible while ordinary managed-app panes change;
- switching sessions changes the presented session content, not the sidebar's
  pane identity, scroll state, size, or placement;
- each row shows session name, active state, and a small window count;
- clicking a row switches the presentation surface to that session;
- right-clicking a row opens a rename interaction;
- renaming commits through a host command rather than mutating host state;
- session create, switch, rename, and close events update the list;
- scrolling preserves guest-owned row identity and selection.

### Command Palette Session Picker

A host-resolved keyboard shortcut opens a command-palette-style picker.

Expected behavior:

- typing filters sessions with fuzzy matching;
- arrow keys or pointer input move the guest-owned selection;
- Enter switches to an existing session;
- a non-empty unmatched query creates and switches to a new named session;
- Escape closes the palette without changing sessions;
- dismissing the palette restores focus according to client policy;
- the palette appears over the current presentation without relocating the
  persistent sidebar.

The existing CLI picker is useful prior art for switch-or-create behavior and
row content, but the experiment must not shell out to it.

## Ownership And Data Flow

```text
session snapshot, pane input, text metrics
                    |
                    v
presentation actor and UI SDK
  own model, components, layout, styling, widget state, and hit testing
                    |
                    | bounded canvas packet
                    v
presentation client
  validates canvas and resource handles
  renders text, paths, images, clips, and layers
  owns pane chrome, final focus, composition, and presentation
```

The SDK may use any internal component model. Its observable host output is a
draw packet, command requests, status, and optional accessibility metadata. The
client never receives `Button`, `List`, `TextInput`, or `SessionRow` nodes.

The actor handles ordinary events and returns without waiting:

```text
Client -> Actor: SessionSnapshot / SessionChanged
Client -> Actor: PaneResized / Focus / Pointer / Key / Text / Ime / Frame
Actor -> Client: SubmitDrawPacket / RequestFrame / CommandRequest / Status
```

The canvas packet is copied and atomically accepted according to
[[../systems/plugin-graphics]]. Guest component trees and caches remain private
and may be mutated after submission.

## SDK Surface Under Test

The experiment should build reusable guest-side functions for at least:

- column, row, stack, padding, and spacing;
- scrollable and virtualized lists;
- text labels using host text layouts;
- buttons and selectable rows;
- a text-input or query-editor behavior;
- overlays or transient palette layers;
- theme tokens and focus/selection styling.

These functions should encode canvas operations such as rectangles, rounded
rectangles, text layouts, clips, transforms, and layers. If a missing primitive
forces host component knowledge, record the concrete workload and first check
whether a smaller canvas, text, input, or SDK facility solves it.

## Commands And Capabilities

The plugin observes session state and requests typed operations. It never
directly mutates the session catalogue, pane placement, focus, or presentation
state.

Required operations are:

```text
SwitchSession(session)
CreateSession(name)
RenameSession(session, name)
OpenSessionPicker
CloseSessionPicker
```

The presentation actor needs authority to observe the attached session and
session list, request those operations, create/update its UI panes, receive pane
input, use native text services, and submit draw packets. Exact manifest names
belong to the capability definitions rather than this experiment.

## Things To Avoid

- Do not introduce a host component tree, widget node ABI, or component patch
  protocol.
- Do not expose WGPU, Smithay, Winit, Wayland, EGL, DMA-BUF, or native toolkit
  handles to WASM.
- Do not make Iced, GPUI, GTK, DOM, or another toolkit type part of the plugin
  ABI.
- Do not move component layout, styling, hit testing, or widget identity into
  the presentation client.
- Do not use the future advanced graphics protocol for ordinary switcher UI.
- Do not treat screenshots or build success alone as behavioral proof.

## Verification Workload

Run automated checks through the Nix-backed built-in test CLI. The proof must
drive real pointer, keyboard, text, and scroll events through a nested client and
record correlated actor and client metrics.

Minimum behavior checks:

- create at least three sessions and verify the sidebar lists them;
- capture the sidebar and active-session content before switching;
- click a row and verify the session changes while sidebar placement and scroll
  state remain stable;
- right-click a row, rename it, and verify the rendered text changes;
- open the palette, type a matching query, navigate, and switch with Enter;
- type an unmatched query and create a session with Enter;
- dismiss with Escape and verify no session change;
- resize the pane and verify component re-layout and text reflow;
- scroll enough rows to exercise clipping and guest-side virtualization;
- replace the actor or recreate the renderer and verify defined recovery output.

Screenshots verify the actual drawing and presentation continuity. Structured
state must also record active session, sidebar/palette state, actor view
revision, accepted packet sequence, input delivery, and presented frame so image
success cannot hide routing or lifecycle failures.

## Acceptance Direction

The experiment succeeds when the UI is concise in guest code, the runtime knows
nothing about its components, and all required behavior is explainable through
canvas, text, pane input, commands, and actor lifecycle. Missing facilities
should be assigned to the narrowest appropriate layer before changing the base
protocol.

Related:

- [[../systems/plugin-ui]]
- [[../systems/plugin-graphics]]
- [[../systems/plugin-advanced-graphics]]
- [[../systems/plugin-runtime]]
- [[../systems/input-shortcuts]]
