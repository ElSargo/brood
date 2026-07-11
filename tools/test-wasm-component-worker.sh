#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guest_manifest="$repo_root/examples/wasm-component-actor/Cargo.toml"
guest_wasm="$repo_root/examples/wasm-component-actor/target/wasm32-unknown-unknown/debug/manifold_component_actor_example.wasm"
component="$repo_root/examples/wasm-component-actor/target/manifold-component-actor.component.wasm"

cd "$repo_root"

nix develop --command cargo build \
  --manifest-path "$guest_manifest" \
  --target wasm32-unknown-unknown

nix develop --command cargo run -p manifold-wasm --example componentize -- \
  "$guest_wasm" \
  "$component"

nix develop --command cargo run -p manifold-wasm --example actor_worker_workload -- \
  "$component"
