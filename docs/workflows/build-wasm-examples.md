---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-07-03
---

# Build WASM Examples

The local examples target `wasm32-unknown-unknown`. They exercise the current prototype WASM lanes, not the final plugin actor/sync-lane API.

```sh
nix develop --command cargo build --manifest-path examples/wasm-key-handler/Cargo.toml --target wasm32-unknown-unknown --release
nix develop --command cargo build --manifest-path examples/wasm-map-hook/Cargo.toml --target wasm32-unknown-unknown --release
nix develop --command cargo build --manifest-path examples/wasm-test-harness/Cargo.toml --target wasm32-unknown-unknown --release
```

The Nix development shell provides the WASM target. The Nix package path also builds and installs packaged WASM example artifacts for the dev workflow.

Related:

- [[../systems/extension-host]]
- [[../systems/plugin-runtime]]
- [[../reference/wasm-fast-lanes]]
