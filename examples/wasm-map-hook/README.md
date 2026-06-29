# WASM Map Hook Example

This is a minimal core-WASM fast map hook for `manifoldd run --wasm-map-hook`.

Build:

```sh
rustup target add wasm32-unknown-unknown
cargo build --manifest-path examples/wasm-map-hook/Cargo.toml --target wasm32-unknown-unknown --release
```

Run:

```sh
manifoldd run \
  --wasm-map-hook examples/wasm-map-hook/target/wasm32-unknown-unknown/release/manifold_wasm_map_hook.wasm
```

This example targets the current experimental map hook and returns candidate
resolved layout items. The layout model is documented in `docs/layout-mould.md`;
the host validates the candidate layout and owns rendering, hit testing, focus,
and input forwarding.
