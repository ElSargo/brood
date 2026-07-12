---
status: target
type: system
authority: product
last-reviewed: 2026-07-13
---

# Plugin UI And Pane Rendering

The target pane UI model provides host-rendered native chrome and rich default components without making those components the limit of plugin graphics. Plugins may describe semantic UI or submit custom render packets; the host owns presentation lifecycle, focus, theme integration, accessibility infrastructure, and final rendering. The low-level graphics protocol is specified in [[plugin-graphics]].

## Ownership

The host owns:

- pane shells, tabs, borders, title bars, menus, overlays, and recovery UI
- layout integration and resize/focus/visibility events
- input routing between native widgets, plugin actors, policy providers, and managed clients
- theming, accessibility, text editing primitives, and platform integration
- validation of plugin-provided UI trees, semantic interaction trees, and render packets

Plugins own:

- pane-local app state
- actor logic for UI events, completions, and commands
- actor-scoped workers for blocking-looking I/O and shared-state concurrent work
- semantic view models or component trees
- custom graphics resources, shaders, and pane render packets when needed

If a plugin actor exits or crashes, the pane identity can remain host-owned. The host can replace the content with native recovery chrome such as restart, close, and logs actions.

## Native Component Path

The default plugin pane should be a host-rendered component tree or patch stream.

Candidate component families:

- root, row, column, stack, scroll
- text, icon, image
- button, checkbox, menu, command bar
- text input and editor primitives
- list, tree, table, virtual list
- divider, spacer, status item
- pane slot or surface slot
- overlay, tooltip, context menu

The component ABI should be Manifold-owned. Backend toolkits such as Iced, GPUI, GTK, or a future renderer may render the accepted tree, but their native types are not the plugin ABI.

## Pane Lifecycle

A plugin actor may request or be assigned a pane. The host creates the pane shell and routes lifecycle messages to the responsible actor.

```text
Host -> Actor: CreatePane { pane_id, kind, constraints, capabilities }
Actor -> Host: SetViewTree or PatchViewTree { pane_id, ... }
Actor -> Host: SubmitRenderPacket { pane_handle, guest_range }
Host -> Actor: PaneResized / PaneFocused / PaneHidden / PaneDestroyed
Actor -> Host: CommandRequest / ViewPatch / Status
```

Semantic pane updates flow through actor mailboxes and host validation. Custom graphics use the bounded render submission path so an actor can batch a pane update in guest memory and cross the WASM boundary once. Neither path runs on the synchronous key/layout path unless a separate policy provider is explicitly involved.

## Native Chrome

Native chrome should be available by default so plugin panes feel coherent across the workspace. The host should provide common UI affordances instead of making every plugin draw its own frame, menus, drag handles, and error surfaces.

Plugin-provided layout or UI may request chrome roles and actions, but the host decides final placement, style, and interaction details.

## Low-Level Graphics Path

Custom drawing is a supported first-class path, although it is not the shortest path for conventional UI. Actors can create host-managed buffers, textures, samplers, shaders, pipelines, render targets, text layouts, and related resources. They submit a bounded, replayable render packet as the latest visual state of a pane.

The protocol can express textured quads, custom geometry, indexed and instanced drawing, arbitrary validated shaders, and offscreen effects. It deliberately does not expose physical devices, swapchains, GPU queues, barriers, fences, or backend-native objects. The host copies and validates a submitted guest-memory range, then owns lowering, synchronization, composition, and presentation.

Canvas, terminal, text-grid, and other domain APIs belong in the SDK above this protocol. Imported client surfaces and external GPU or shared-buffer paths remain separately negotiated capabilities with explicit lifecycle rules. See [[plugin-graphics]] for the complete graphics and text contract.

## Relation To Policy Providers

UI actor dispatchers are completion-driven and must not block. They may delegate
I/O or long-running work to workers and handle the resulting completion
messages. Actors can publish latest state when a policy provider needs that
state for keyboard or layout decisions, but the view tree itself should not
become a policy-path backlog.

For example, a session switcher actor can own picker query state and render a native component tree. A host shortcut or key policy provider can open the picker immediately by sending a message or host directive, while the actor performs filtering and UI updates through normal mailbox delivery.

## Validation And Diagnostics

The host should validate:

- stable IDs for stateful widgets
- bounded node, property, string, and action counts
- capability use for commands, files, process launch, clipboard, and cross-plugin messages
- unsupported component features by backend
- patch application order and generation

Diagnostics should report rejected UI updates, backend feature adaptation, actor latency, frame/update counters, and recovery state. Render-packet, shader, resource, and presentation diagnostics are specified in [[plugin-graphics]].

Related:

- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[plugin-graphics]]
- [[input-shortcuts]]
- [[layout-policy]]
- [[../concepts/pane]]
- [[../experiments/session-switcher-ui]]
