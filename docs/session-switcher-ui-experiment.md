# Session Switcher UI Experiment

This document defines the shared target for experimental native UI backends.
It is a target design for the experiment, not a statement that the interfaces
exist today.

The first backend spikes should implement the same session switcher UI against
the same host/plugin boundary, then record where each backend fits or fights the
model. The initial backend candidates are Iced and GPUI.

## Goal

Build a native session switcher UI whose behavior is driven by a WASM-owned
state model and a host-rendered UI tree.

The experiment should answer:

- whether a flat WASM UI IR can describe real Manifold shell UI
- whether the host can validate and render that IR without exposing compositor
  internals to the module
- whether backend-specific event, focus, layout, and rendering models can
  support the same pane behavior
- which backend quirks should shape the permanent UI/runtime API

## Target State

The target user-facing feature has two native UI panes.

### Persistent Session Sidebar

The presentation surface has a persistent scrollable sidebar. It lists every
surface session known to the active workspace.

Expected behavior:

- the sidebar remains visible while ordinary managed app panes change
- the sidebar remains in the same presentation location before, during, and
  after a session switch
- switching sessions changes the presented session content, not the sidebar's
  pane identity, scroll state, size, or placement
- each row shows enough state to identify the session, including name, active
  state, and a small window count
- clicking a row switches the presentation surface to that session
- right-clicking a row opens a rename interaction for that session
- renaming commits through a host command, not by directly mutating host state
- the active session row updates when sessions are created, switched, renamed,
  or closed
- the list scrolls without losing stable row identity or active selection

The sidebar is surface-local presentation UI. It observes the session list from
the host, but the host remains authoritative for the list and for the active
surface session.

### Command Palette Session Picker

A keyboard shortcut opens a command-palette-style picker for sessions.

Expected behavior:

- the palette appears as transient native UI over the current presentation
- typing filters sessions with fuzzy matching
- arrow keys or pointer input can move the selected result
- Enter switches to the selected existing session
- a non-empty query with no selected existing session creates and switches to a
  new named session
- Escape closes the palette without changing the active session
- the palette restores focus according to host policy when dismissed
- opening, using, or dismissing the palette must not replace or relocate the
  persistent sidebar

The existing CLI session picker behavior in `crates/manifoldd/src/control.rs`
is useful prior art for row formatting, switch-or-create behavior, and session
list data, but this experiment should not shell out to the CLI picker.

## Architecture

The experiment should keep this ownership model:

```text
WASM plugin
  owns switcher-local state
  computes view state
  writes a flat UI tree into guest memory
        |
        v
Manifold host
  reads and validates the UI tree
  owns session truth, commands, focus, input routing, layout, and rendering
  lowers the accepted tree into the selected native backend
        |
        v
Native backend
  renders accepted nodes
  reports backend input/focus/widget events back to the host
```

The backend must not become the plugin ABI. Iced, GPUI, or a later renderer may
be used to render the accepted UI tree, but the module boundary is Manifold's
own data model.

## WASM Boundary

Use the current Wasmtime style of reading guest linear memory as the mental
model. Avoid designing a bidirectional shared mutable region for the first pass.

Preferred lifecycle:

```text
host creates surface-realm switcher module
host sends UiHostEvent::Snapshot with the current session list
host calls plugin_update(event)
plugin mutates its state
plugin returns BufferRef to a full UI tree or patch
host reads, validates, commits, and renders
```

Minimum exported functions:

```text
manifold_ui_init(config_ptr, config_len) -> Status
manifold_ui_update(event_ptr, event_len) -> BufferRef
manifold_ui_view() -> BufferRef
```

`BufferRef` should describe an immutable guest-memory range after the call
returns:

```text
BufferRef {
  ptr: u32,
  len: u32,
  generation: u32,
}
```

The host must copy or finish reading the buffer before making another call that
may let the guest reuse memory.

## UI IR

The first IR should be flat, bounded, and index-addressed. Do not expose host
object pointers, Rust widget objects, compositor surfaces, renderer handles, or
backend-specific types.

Candidate binary layout:

```text
UiBuffer
  Header
  Node[]
  ChildIndex[]
  Prop[]
  StringTable
  Action[]
```

Candidate node shape:

```text
Node {
  id: UiId,
  kind: UiNodeKind,
  first_child: u32,
  child_count: u32,
  first_prop: u32,
  prop_count: u32,
}
```

Candidate node kinds:

```text
Root
Column
Row
Stack
Scroll
Text
Button
TextInput
List
ListItem
ContextMenu
Divider
Spacer
Icon
```

The switcher experiment may add a `SessionRow` helper node if that materially
reduces duplicated backend glue, but it should first try to express rows with
ordinary nodes.

Candidate property families:

```text
layout: width, height, min_width, min_height, grow, gap, padding
style: role, emphasis, selected, disabled, danger
text: string_ref, placeholder_ref
input: value_ref, cursor, selection
list: active_index, item_count
action: action_id, event_kind
identity: session_id, pane_id, stable_key
```

Use stable `UiId` values for nodes that need widget state, focus restoration,
scroll retention, or event targeting.

## Events And Actions

Backend events should be translated into host-level UI events before entering
WASM.

Candidate events:

```text
UiHostEvent::SessionSnapshot {
  active_session: SessionId,
  sessions: Vec<SessionSummary>,
}

UiHostEvent::Click {
  target: UiId,
  button: PointerButton,
}

UiHostEvent::TextChanged {
  target: UiId,
  value: String,
}

UiHostEvent::Key {
  key: UiKey,
  modifiers: UiModifiers,
}

UiHostEvent::Dismiss {
  reason: DismissReason,
}
```

The plugin may request semantic actions. The host validates and executes them.

Required actions for this experiment:

```text
SwitchSession(SessionId)
CreateSession { name: String }
RenameSession { session: SessionId, name: String }
OpenSessionPicker
CloseSessionPicker
SetPickerQuery(String)
MovePickerSelection { direction: Previous | Next }
```

Do not let the plugin mutate the session list directly. Session list changes
must arrive back as host snapshots or host events after a command commits.

## Realm And Capabilities

The session switcher belongs in a surface realm. It is presentation-local and
follows one presentation surface even when that surface switches attached
sessions.

Minimum capabilities:

```text
surface:observe-attached-session
surface:switch-session
surface:create-session
surface:rename-session
ui:render-tree
ui:receive-input
```

The host may implement the first spike with native structs instead of a real
manifest, but the experiment should keep capability checks visible in the code
shape and notes.

## Backend Requirements

Each backend implementation should:

- consume the same accepted UI tree shape
- keep Manifold host state authoritative
- preserve stable widget identity across re-rendered trees where the backend
  supports it
- support scrollable sidebar contents
- support a focused text input in the palette
- support pointer click, right-click, keyboard navigation, Enter, and Escape
- keep the palette transient and dismissible
- report rejected, ignored, or adapted UI features in logs or status
- expose enough status to verify active session, visible sidebar rows, palette
  open state, query, selected result, and backend frame/update counters

Backend-specific helper code is allowed. Backend-specific semantics in the IR
are not.

## Things To Avoid

- Do not expose Smithay, Wayland, EGL, DMA-BUF, renderer, or backend handles to
  WASM.
- Do not make Iced, GPUI, or any other toolkit type part of the plugin ABI.
- Do not let a plugin directly mutate session, pane, focus, or presentation
  state.
- Do not model panes as owned by the presentation surface.
- Do not require a WebView, GTK, Qt, or external process for this experiment.
- Do not start with a fully general CSS, layout solver, DOM, or arbitrary
  scripting layer.
- Do not rely on stringly ad hoc parsing where a compact typed structure is
  practical.
- Do not treat screenshots or build success alone as proof that behavior works.

## Verification Workload

Prefer the built-in test CLI for automated checks. The backend branches should
use `cargo run -- test` or `target/debug/manifoldd test nested ...` paths rather
than starting a host-visible presentation from an existing desktop session.

Minimum behavior checks:

- create at least three sessions
- verify the sidebar lists them and marks the active one
- capture a screenshot before switching sessions that shows the sidebar and
  current session content
- click a sidebar row and verify the active session changes while the sidebar
  remains in the same place
- capture a screenshot after the sidebar switch and compare it with the
  before-switch screenshot
- right-click a sidebar row, rename it, and verify the new name is visible
- open the palette with the configured shortcut
- capture a screenshot with the palette open over the current presentation
- type a query that matches one existing session and switch with Enter, then
  verify with status and a screenshot that the session changed while the
  sidebar remained in place
- type a query that matches no session and create a new session with Enter,
  then verify with status and a screenshot that the new session is active while
  the sidebar remained in place
- dismiss the palette with Escape and verify no session change occurs
- sample status before and after each operation

Screenshots are required for this experiment because the main behavior under
test is presentation continuity. Structured state from the nested client and
`manifoldd` is also required so screenshots are not the only proof.

## Backend Notes Requirement

Each backend branch must add a short implementation note before handoff:

```text
docs/experiments/session-switcher-ui/<backend>.md
```

The note should include:

- what was implemented
- how to run it
- what was verified
- what failed or was left incomplete
- backend quirks
- places the Manifold UI IR model did not fit cleanly
- places the backend wanted to own lifecycle, focus, input, layout, or rendering
- recommended changes to the shared IR or host/plugin boundary

Be direct about model mismatches. A failed or awkward backend is useful if it
explains why.

## Open Questions

- whether the first committed UI result should always be a full tree or may be
  a patch
- whether fuzzy matching belongs in the plugin, host, or both
- whether text editing state should be owned by the host widget backend,
  mirrored in WASM state, or split by field type
- whether right-click rename should be an inline editor, context menu, or modal
- how much of the final theme system should appear in the first IR
- which status fields are needed to make backend experiments easy to compare
