# Component actor and worker example

This guest demonstrates both sides of the plugin I/O model:

- `dispatch` submits completion I/O and returns immediately;
- `worker-run` awaits the worker file API through the Component Model async
  ABI, which suspends only that worker task;
- CPU-running stackless workers await `worker-yield` at cooperative scheduling
  points so the shared component Store is available to the actor dispatcher.
- guest-owned shared state uses release/acquire atomics plus host notifications;
  workers use bounded `worker-wait-any`, while actors receive one-shot
  coalesced notification events.

Build and componentize it from the repository root:

```sh
nix develop --command cargo build \
  --manifest-path examples/wasm-component-actor/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
nix develop --command cargo run -p manifold-wasm --example componentize -- \
  examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.wasm \
  examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.component.wasm
```

This component intentionally imports no standard WASI interfaces. That makes
the actor/worker type split enforceable: dispatch has only the nonblocking
completion calls, and the async file/yield calls reject use outside a registered
worker task. The Preview 1 compatibility example is the runnable path for
ordinary Rust `std::io` and libc-style worker calls.

`nix build .#wasm-examples` performs the same encoding in the Nix sandbox and
installs only the runtime-ready component, not its intermediate core module.
Load that component through the normal plugin runtime when validating its
user-visible behavior.
