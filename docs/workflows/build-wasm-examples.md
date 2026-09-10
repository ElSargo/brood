---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-08-15
code-links:
  - tools/brood-build.sh
  - tools/brood-build.mk
---

# Build WASM Examples

The repository contains several kinds of WASM artifact. Build each with the target
expected by its runtime instead of treating every `.wasm` file as interchangeable.

For normal development, build all stale host and WASM artifacts through the
aggregate builder:

```sh
nix develop --command ./tools/brood-build.sh
```

Cargo remains responsible for dependency-aware freshness. Guest workspaces share a dependency cache and run in sequence while the host
build overlaps them. Component Model encoding is skipped when
the validated component is newer than its core module. The default client
package is then co-located with its root component for direct use by the
repository configuration. The builder's `--help` output defines its exact
modes and concurrency controls.

The core-module examples and the default workspace actor target
`wasm32-unknown-unknown`:

```sh
nix develop --command cargo build --manifest-path examples/wasm-key-handler/Cargo.toml --target wasm32-unknown-unknown --release
nix develop --command cargo build --manifest-path plugins/default-workspace/Cargo.toml --target wasm32-unknown-unknown --release
```

The component actor is compiled without standard WASI imports and then encoded
as a Component Model artifact. This keeps its dispatcher completion-only; the
separate Preview 1 backend supplies ordinary libc/WASI compatibility to workers:

```sh
nix develop --command cargo build \
  --manifest-path examples/wasm-component-actor/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
nix develop --command cargo run -p brood-wasm --example componentize -- \
  examples/wasm-component-actor/target/wasm32-unknown-unknown/release/brood_component_actor_example.wasm \
  examples/wasm-component-actor/target/wasm32-unknown-unknown/release/brood_component_actor_example.component.wasm
```

The Preview 1 compatibility worker remains a core
`wasm32-wasip1-threads` module. Its shared-memory import and private worker
instances are the compatibility runtime contract, so do not componentize it.
The checked-in linker configuration declares a 512 MiB maximum, large enough
for the workload while remaining inside the runtime's aggregate per-generation
guest-memory ceiling.

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
result/examples/wasm-component-actor/target/wasm32-unknown-unknown/release/brood_component_actor_example.component.wasm
result/examples/wasm-preview1-worker/target/wasm32-wasip1-threads/release/brood_preview1_worker_example.wasm
```

These artifacts are examples and real plugin workloads, not a synthetic
in-process correctness suite. Product acceptance should load them through the
normal runtime path and judge their externally visible output.

Related:

- [[../systems/extension-host]]
- [[../targets/systems/plugin-runtime]]
- [[../reference/wasm-fast-lanes]]
