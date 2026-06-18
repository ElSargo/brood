# WASM Key Handler Example

This is a minimal core-WASM normal-mode key handler for `manifoldd --wasm-key-handler`.

Build:

```sh
rustup target add wasm32-unknown-unknown
cargo build --manifest-path examples/wasm-key-handler/Cargo.toml --target wasm32-unknown-unknown --release
```

Run:

```sh
manifoldd run \
  --wasm-key-handler examples/wasm-key-handler/target/wasm32-unknown-unknown/release/manifold_wasm_key_handler.wasm
```

Bindings:

- `Ctrl-O`: spawn `kitty`
- `Ctrl-Q`: close the active window
