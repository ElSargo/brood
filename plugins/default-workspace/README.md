# Default Workspace Plugin

This is the target-shape replacement for the old lane-specific WASM examples:

- `examples/wasm-key-handler`: now exported as `manifold_policy_provider_key_workspace`.
- `examples/wasm-map-hook`: replaced by message-based actor layout.

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
