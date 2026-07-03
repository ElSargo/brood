# WASM Key Handler Example

This is a minimal core-WASM key handler for `manifold run --wasm-key-handler`.
The example keeps app-id navigation policy in the guest: it looks up panes by
app id, calls exact pane focus, and tracks when no content pane is focused.

Build:

```sh
rustup target add wasm32-unknown-unknown
cargo build --manifest-path examples/wasm-key-handler/Cargo.toml --target wasm32-unknown-unknown --release
```

Run:

```sh
manifold run \
  --wasm-key-handler examples/wasm-key-handler/target/wasm32-unknown-unknown/release/manifold_wasm_key_handler.wasm
```

For the exact CLI argument definition, see `crates/manifoldd/src/main.rs`.

Default keymap:

- `Ctrl-J`: focus next pane without leaving passthrough mode
- `Ctrl-K`: focus previous pane without leaving passthrough mode
- `Ctrl-G`: focus the Foot LazyGit pane, spawning it if needed
- `Ctrl-Enter`: focus the Foot pi agent pane, spawning it if needed
- `Ctrl-1` through `Ctrl-5`: focus numbered Foot terminal panes, spawning them if needed
- `Ctrl-O`: spawn `foot`
- `Ctrl-N`: create and attach a new surface session
- `Ctrl-Q`: close the active pane without leaving passthrough mode
- `Ctrl-Shift-Q`: close the active surface session
- `Ctrl-F`: toggle managed fullscreen
- `Ctrl-L`: split active pane right
- `Ctrl-H`: split active pane down
- `Ctrl-S`: split active pane using the active slot's wider axis
- `Ctrl-U`: remove the active split
- `Ctrl-.`: focus the next split slot
- `Ctrl-,`: focus the previous split slot
- `Ctrl-D`: toggle bottom dock
- `Ctrl-P`: toggle left and right docks
- `Alt-Space`: switch the active surface to the next session
