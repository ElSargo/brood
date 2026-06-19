# WASM Key Handler Example

This is a minimal core-WASM normal-mode key handler for `manifoldd run --wasm-key-handler`.

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

For the exact CLI argument definition, see `crates/manifoldd/src/main.rs`.

Default keymap:

- `Ctrl-O`: spawn `kitty`
- `Ctrl-J`: focus next pane
- `Ctrl-K`: focus previous pane
- `Ctrl-Q`: close the active pane
- `Ctrl-F`: toggle managed fullscreen
- `Ctrl-L`: split active pane right
- `Ctrl-H`: split active pane down
- `Ctrl-D`: toggle bottom dock
- `Ctrl-P`: toggle left and right docks
