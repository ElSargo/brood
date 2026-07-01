---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-07-01
---

# Build WASM Examples

The local examples target `wasm32-unknown-unknown`.

```sh
rustup target add wasm32-unknown-unknown
cargo build --manifest-path examples/wasm-key-handler/Cargo.toml --target wasm32-unknown-unknown --release
cargo build --manifest-path examples/wasm-map-hook/Cargo.toml --target wasm32-unknown-unknown --release
cargo build --manifest-path examples/wasm-test-harness/Cargo.toml --target wasm32-unknown-unknown --release
```

The Nix package path also builds and installs packaged WASM example artifacts for the dev workflow.

Related:

- [[../systems/extension-host]]
- [[../reference/wasm-fast-lanes]]
