---
status: target
type: system
authority: product
last-reviewed: 2026-07-03
---

# Plugin UI And Pane Rendering

The target pane UI model favors host-rendered native chrome and rich default components. Plugins describe UI and behavior; the host owns presentation lifecycle, focus, theme, accessibility, and final rendering.

## Ownership

The host owns:

- pane shells, tabs, borders, title bars, menus, overlays, and recovery UI
- layout integration and resize/focus/visibility events
- input routing between native widgets, plugin actors, policy providers, and managed clients
- theming, accessibility, text editing primitives, and platform integration
- validation of plugin-provided UI trees or patches

Plugins own:

- pane-local app state
- actor logic for UI events and commands
- semantic view models or component trees
- optional custom drawing resources when granted

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
Host -> Actor: PaneResized / PaneFocused / PaneHidden / PaneDestroyed
Actor -> Host: CommandRequest / ViewPatch / Status
```

Pane content updates flow through actor mailboxes and host validation. They do not run on the synchronous key/layout path unless a separate policy provider is explicitly involved.

## Native Chrome

Native chrome should be available by default so plugin panes feel coherent across the workspace. The host should provide common UI affordances instead of making every plugin draw its own frame, menus, drag handles, and error surfaces.

Plugin-provided layout or UI may request chrome roles and actions, but the host decides final placement, style, and interaction details.

## Custom Drawing Escape Hatches

Some plugins need lower-level rendering. The target model can support explicit escape hatches, each guarded by capabilities and lifecycle rules:

- canvas-like retained drawing primitives
- terminal or text grid surface
- host-managed texture/resource handles
- imported external client surfaces
- later GPU or shared-buffer paths if justified

Custom drawing should not be the default pane API. It should bypass native components only when real workloads need it.

## Relation To Policy Providers

UI actors are asynchronous. They can publish latest state to publications when a policy provider needs that state for keyboard or layout decisions, but the view tree itself should not become a policy-path backlog.

For example, a session switcher actor can own picker query state and render a native component tree. A host shortcut or key policy provider can open the picker immediately by sending a message or host directive, while the actor performs filtering and UI updates through normal mailbox delivery.

## Validation And Diagnostics

The host should validate:

- stable IDs for stateful widgets
- bounded node, property, string, and action counts
- capability use for commands, files, process launch, clipboard, and cross-plugin messages
- unsupported component features by backend
- patch application order and generation

Diagnostics should report rejected UI updates, backend feature adaptation, actor latency, frame/update counters, and recovery state.

Related:

- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[input-shortcuts]]
- [[layout-policy]]
- [[../concepts/pane]]
- [[../experiments/session-switcher-ui]]
