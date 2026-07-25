# Session Switcher Plugin

This is the default presentation-local Brood shell plugin. The same
Component Model artifact runs first as the blessed client init actor and starts
the child actor which owns the native session-switcher pane, the reusable
`picker` actor, and the initial coding-agent UI. The init actor also starts and
supervises more coding-agent UI children, so adding the agent
workload does not replace the default shell. The switcher child receives typed
session snapshots from `brood-client`, renders them through canvas packets,
and requests typed session operations without DBus, subprocesses, or a native
toolkit inside the plugin.

When the active surface session changes, the init actor rebinds every coding
agent UI child to that session's independently supervised agent registry.

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

The init actor also owns the diff viewer's pane lifecycle. `Ctrl+Alt+D` starts
the diff actor with narrow read-only workspace and `subprocess:git`
capabilities; the next press stops the actor and therefore closes the pane. The
diff viewer stays an ordinary workspace pane and does not participate in the
switcher's root layout policy.

The pane supports:

- project-grouped, collapsible session lists;
- a project-heading action which creates a branch, managed Git worktree, and
  active session;
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
