# Session Switcher Plugin

This is the default presentation-local Manifold shell plugin. The same
Component Model artifact runs first as the blessed client init actor and starts
one child actor which owns a native plugin pane. The child receives typed
session snapshots from `manifold-client`, renders them through canvas packets,
and requests typed session operations without DBus, subprocesses, or a native
toolkit inside the plugin.

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
nix develop --command cargo run -p manifold-wasm --example componentize -- \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/manifold_session_switcher_plugin.wasm \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/manifold_session_switcher_plugin.component.wasm
```

The repository `manifold.toml` selects the component as `[init].client`.
Exact client recovery and override flags remain defined in the CLI parser.
