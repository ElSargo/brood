# Default Workspace Plugin

This is the target-shape replacement for the old lane-specific WASM examples:

- `examples/wasm-key-handler`: now exported as `manifold_policy_provider_key_workspace`.
- `examples/wasm-map-hook`: now exported as the client-local
  `manifold_policy_provider_layout_tile` flat-layout provider. The module also
  retains its layout actor for the legacy backend host while the presentation
  client migration is in progress.

It is intentionally a standalone WASM crate for now. The current `manifold-wasm` crate defines and hosts the new ABI, but it does not yet provide a guest-side `no_std` SDK, so the plugin exports the ABI directly. Policy providers write their response into guest-owned memory and return a `PolicyProviderResponse` descriptor pointer; the host copies that response while the guest is suspended.

Build:

```sh
cargo build --manifest-path plugins/default-workspace/Cargo.toml --target wasm32-unknown-unknown --release
```

Output:

```text
plugins/default-workspace/target/wasm32-unknown-unknown/release/manifold_default_workspace_plugin.wasm
```

Run the key policy in a nested subject:

```sh
nix develop --command cargo run -- test nested --force-outer --inner-headless \
  --inner-wasm-key-policy workspace \
  plugins/default-workspace/target/wasm32-unknown-unknown/release/manifold_default_workspace_plugin.wasm
```

The repository's observable acceptance proof uses this exact policy with a
real terminal, injected key press/release events, modifier-state controls, and
a compositor screenshot. Bare `B` leaves `Ctrl+O` forwarded; `Ctrl+B` changes
the same policy context so `Ctrl+O` visibly opens exactly one second terminal.
Run `tools/test-typed-key-policy-nested.sh` through the checked-in Nix shell.
