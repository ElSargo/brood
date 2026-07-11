---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-07-11
---

# Build WASM Examples

The repository contains three kinds of WASM artifact. Build each with the target
expected by its runtime instead of treating every `.wasm` file as interchangeable.

The legacy lane-specific examples are core modules targeting
`wasm32-unknown-unknown`. They remain useful as isolated ABI examples, but the
current config loader does not load their old lane kinds:

```sh
nix develop --command cargo build --manifest-path examples/wasm-key-handler/Cargo.toml --target wasm32-unknown-unknown --release
nix develop --command cargo build --manifest-path examples/wasm-map-hook/Cargo.toml --target wasm32-unknown-unknown --release
nix develop --command cargo build --manifest-path examples/wasm-test-harness/Cargo.toml --target wasm32-unknown-unknown --release
```

The component actor is compiled without standard WASI imports and then encoded
as a Component Model artifact. This keeps its dispatcher completion-only; the
separate Preview 1 backend supplies ordinary libc/WASI compatibility to workers:

```sh
nix develop --command cargo build \
  --manifest-path examples/wasm-component-actor/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
nix develop --command cargo run -p manifold-wasm --example componentize -- \
  examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.wasm \
  examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.component.wasm
```

The Preview 1 compatibility worker remains a core
`wasm32-wasip1-threads` module. Its shared-memory import and private worker
instances are the compatibility runtime contract, so do not componentize it:

```sh
nix develop --command cargo build \
  --manifest-path examples/wasm-preview1-worker/Cargo.toml \
  --target wasm32-wasip1-threads \
  --release
```

The Nix development shell provides all three Rust targets and `wasm-tools`.
Build the sandboxed package to produce the same runtime-ready artifacts used by
the development workflow:

```sh
nix build .#wasm-examples
```

The package vendors the locked guest dependencies, encodes and validates the
component with `wasm-tools` inside the sandbox, and installs these new artifacts
at:

```text
result/examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.component.wasm
result/examples/wasm-preview1-worker/target/wasm32-wasip1-threads/release/manifold_preview1_worker_example.wasm
```

Use `tools/test-wasm-component-worker.sh` and
`tools/test-wasm-preview1-worker.sh` for the corresponding live runtime
workloads. The Preview 1 script exercises both the low-level worker runtime and
the daemon-compatible actor facade in-process.

The Nix check builds both host workloads and both guest artifacts in the
sandbox, then executes the notification, select, completion-I/O, cancellation,
ordinary-WASI, and structured-shutdown proofs:

```sh
nix build .#wasm-io-check
```

The packaged Rube Goldberg runner supplies those same two guest artifacts to a
multi-actor workload. `quick` is the deterministic CI profile, `stress` is the
larger finite profile, and `soak` repeats deterministic epochs until its
configured bound:

```sh
nix run .#wasm-io-rube -- --profile quick --out-dir ./rube-quick
nix run .#wasm-io-rube -- --profile stress --seed 5566816691025824836 --out-dir ./rube-stress
nix run .#wasm-io-rube -- --profile soak --seed 5566816691025824836 --out-dir ./rube-soak
```

For a source-tree run that rebuilds the packaged guests first, use
`tools/test-wasm-io-rube-goldberg.sh`. Its exact development overrides are
defined by that script. Profile, seed, and output directory belong to the
wrapper before `--`; actor, worker, step, epoch, and duration overrides after
`--` are forwarded to the Rust runner. The wrapper applies a profile-specific
hard timeout, configurable with `MANIFOLD_WASM_IO_RUBE_TIMEOUT_SECONDS`.
Preserve its reported output directory and replay line when reproducing a
failure.

For the end-to-end daemon proof, run:

```sh
nix develop --command bash tools/test-wasm-preview1-worker-nested.sh
nix develop --command bash tools/test-wasm-io-registry-nested.sh
```

Those workloads use `manifold test nested` with an offscreen outer presentation.
The Preview 1 workload
loads the compatibility actor through the daemon plugin path, sends mailbox
traffic through the normal control surface, proves actor completion I/O while
workers select over a guest-owned shared-memory queue, retains an ordinary
worker `read` compatibility case, and hot-reloads the actor while two worker
wait sets are pending. It verifies that the old generation's notification,
pollable, and wait-registration state is gone before the replacement handles
traffic. The registry workload hosts eight independently addressed Component
and Preview 1 actors, replaces one suspended generation, and proves its seven
peers continue without losing their state.

Related:

- [[../systems/extension-host]]
- [[../systems/plugin-runtime]]
- [[../reference/wasm-fast-lanes]]
