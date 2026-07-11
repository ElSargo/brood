#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guest_manifest="$repo_root/examples/wasm-preview1-worker/Cargo.toml"
guest_wasm="$repo_root/examples/wasm-preview1-worker/target/wasm32-wasip1-threads/release/manifold_preview1_worker_example.wasm"

cd "$(dirname "$guest_manifest")"
nix develop "$repo_root" --command cargo build --quiet \
  --manifest-path "$guest_manifest" \
  --target wasm32-wasip1-threads \
  --release

cd "$repo_root"
nix develop --command cargo run --quiet -p manifold-wasm \
  --example preview1_worker_workload -- \
  "$guest_wasm"
