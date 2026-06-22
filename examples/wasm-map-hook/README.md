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

The sample policy keeps the first visible pane as the left master pane and places every other visible pane in a right-side stack.
