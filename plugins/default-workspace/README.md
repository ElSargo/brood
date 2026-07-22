# Default Workspace Plugin

This is the target-shape workspace module:

- `manifold_policy_provider_key_workspace` performs the bounded synchronous key
  decision.
- `manifold_actor_workspace_epoch_run` receives layout snapshots and actions
  through its normal mailbox, maintains layout state, and submits resolved
  layouts through its ordinary host outbox.

It is intentionally a standalone WASM crate for now. The current
`manifold-wasm` crate defines and hosts the ABI, but it does not yet provide a
guest-side `no_std` SDK, so the plugin exports the ABI directly. The key
provider returns its decision through guest-owned memory. The layout actor uses
the normal actor inbox and outbox ABI.

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

The repository's observable acceptance proof uses this exact module with a
real terminal, injected key press/release events, modifier-state controls, and
a compositor screenshot. Bare `B` leaves `Ctrl+O` forwarded; `Ctrl+B` changes
the same policy context so `Ctrl+O` visibly opens exactly one second terminal.
Run `tools/test-typed-key-policy-nested.sh` through the checked-in Nix shell.
