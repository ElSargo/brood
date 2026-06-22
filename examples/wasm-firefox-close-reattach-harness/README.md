This core-WASM harness drives the Firefox close/reattach regression through
Manifold's async test-harness imports.

It is intended to run through the Nix package/check:

```sh
nix build .#firefoxCloseReattachTest
```

The harness runs in the outer nested supervisor and controls the inner system
under test through `nested-inner` control commands.
