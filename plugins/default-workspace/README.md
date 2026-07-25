# Default Workspace Plugin

This is the target-shape workspace module:

- `brood_policy_provider_key_workspace` performs the bounded synchronous key
  decision.
- `brood_actor_workspace_epoch_run` receives layout snapshots and actions
  through its normal mailbox, maintains layout state, and submits resolved
  layouts through its ordinary host outbox.

It is intentionally a standalone WASM crate for now. The current
`brood-wasm` crate defines and hosts the ABI, but it does not yet provide a
guest-side `no_std` SDK, so the plugin exports the ABI directly. The key
provider returns its decision through guest-owned memory. The layout actor uses
the normal actor inbox and outbox ABI.

Build:

```sh
cargo build --manifest-path plugins/default-workspace/Cargo.toml --target wasm32-unknown-unknown --release
```

Output:

```text
plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm
```

Run the key policy in a nested subject:

```sh
nix develop --command cargo run -- test nested --force-outer --inner-headless \
  --inner-wasm-key-policy workspace \
  plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm
```

The normal config selects this provider for the presentation client. Its
always-available workspace chords are `Ctrl+Alt+M` for the session-switcher
pane, `Ctrl+J` / `Ctrl+K` for managed-window focus, and `Ctrl+O` for a new
`foot` window. The switcher chord emits an ordinary action message to the
`session-switcher` actor; the policy does not toggle client-owned pane state.
The repository acceptance proof uses this exact module with real terminal
surfaces, injected key press/release events, and compositor screenshots. Run
`tools/test-session-switcher-plugin-nested.sh` through the checked-in Nix shell.
