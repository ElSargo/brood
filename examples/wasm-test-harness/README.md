This is a minimal async core-WASM test harness for `manifoldd run --test-harness`.

Build:

```sh
rustup target add wasm32-unknown-unknown
cargo build --manifest-path examples/wasm-test-harness/Cargo.toml --target wasm32-unknown-unknown --release
```

Run:

```sh
manifoldd run \
  --test-harness examples/wasm-test-harness/target/wasm32-unknown-unknown/release/manifold_wasm_test_harness.wasm
```

The sample harness polls `status-json` until the presentation attaches. It returns:

- `0` on pass
- `1` on failure
- `2` on pause for interactive inspection
