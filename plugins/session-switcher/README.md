# Session Switcher Plugin

This is the default presentation-local Brood shell plugin. The same
Component Model artifact runs first as the blessed client init actor and starts
the child actor which owns the native session-switcher pane, the reusable
`picker` actor, global inbox, keybinding editor, and compact status bar. A fresh
presentation creates no surface session or terminal UI; creating or selecting
a session starts session-scoped workloads. The init actor also starts and
supervises diff, terminal, and coding-agent UI children, so adding those
workloads does not replace the default shell. The switcher child receives
typed session snapshots from `brood-client`, renders them through canvas
packets, and requests typed session operations without DBus, subprocesses, or
a native toolkit inside the plugin.

The init actor gives every child an explicit presentation scope in its start
request. The switcher, picker, keybinding editor, and inbox use `global`;
terminal, editor, and coding-agent panes use a backend-qualified `session`
scope. Changing the active session changes which scoped children are visible;
it does not infer ownership from the actor name or rebind an existing pane to
another session. A session and its scoped children remain alive with no managed
Wayland windows and stop only when the session is explicitly closed.

The child claims the root layout lease, keeps its own pane in a right sidebar,
and asks the host to delegate every other pane through its `rest` slot to the
configured `workspace` layout actor. Hiding the switcher removes its pane item
and expands `rest` to the full viewport; showing it restores the sidebar without
replacing the workspace actor or its private layout state.

Sidebar visibility and focus are actor state. The default key policy delivers
`Ctrl+Alt+M` as an ordinary `sidebar-action` mailbox message; the plugin answers
by publishing another fragment for its existing root lease.

`Ctrl+Alt+P` sends the switcher a second ordinary action. The switcher creates a
request on the reusable picker actor and activates the session returned in the
selection reply. The picker pane is a centered overlay in this actor's root
layout and is never delegated into the workspace subtree.

`Ctrl+Alt+A` sends the init actor an agent-picker action. The picker enumerates
the current session's registry, and selecting an entry starts a coding-agent
UI already bound to that session and agent. The UI remains independently
addressed and its tab bar continues to follow the whole session registry.

The init actor also owns the diff viewer's pane lifecycle. `Ctrl+Alt+D` starts
one diff actor per session with narrow read-only workspace and
`subprocess:git` capabilities, or focuses that actor when it is already open.
The diff viewer stays an ordinary workspace pane and does not participate in
the switcher's root layout policy.

`Ctrl+Alt+T` creates a new paired terminal session actor and terminal UI; there
is no per-session terminal limit. `Ctrl+Alt+Q` closes the focused managed or
plugin pane. Closing a plugin pane stops its presentation actor and performs
the corresponding session-side cleanup where required. The reusable picker
and switcher use their own hide/cancel lifecycle instead of being destroyed.

The pane supports:

- project-grouped, collapsible session lists;
- a project-heading action which creates a branch, managed Git worktree, and
  active session through an addressed daemon-actor request rather than DBus;
- pointer or keyboard session selection and activation;
- search-as-you-type filtering;
- creating an unmatched session with Enter;
- renaming the selected session with F2 or right click;
- closing the selected session with Delete or the row close button;
- mouse-wheel scrolling while preserving selection.

Build and componentize it through the checked-in Nix environment:

```sh
nix develop --command cargo build \
  --manifest-path plugins/session-switcher/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
nix develop --command cargo run -p brood-wasm --example componentize -- \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.wasm \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.component.wasm
nix develop --command cargo build \
  --manifest-path plugins/picker/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
nix develop --command cargo run -p brood-wasm --example componentize -- \
  plugins/picker/target/wasm32-unknown-unknown/release/brood_picker_plugin.wasm \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_picker_plugin.component.wasm
```

The repository `brood.toml` selects the component as `[init].client`.
Exact client recovery and override flags remain defined in the CLI parser.
