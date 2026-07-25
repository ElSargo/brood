# Session Switcher Plugin

This is the default presentation-local Brood shell plugin. The same
Component Model artifact runs first as the blessed client init actor and starts
the child actor which owns the native session-switcher pane. The init actor
also starts and supervises coding-agent UI children, so adding the agent
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

The pane supports:

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
```

The repository `brood.toml` selects the component as `[init].client`.
Exact client recovery and override flags remain defined in the CLI parser.
