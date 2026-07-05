# Plugins

Root-level target-shape Manifold plugins live here.

- `default-workspace/` ports the existing prototype key handler and map/layout hook into the new `manifold-wasm` ABI shape: exported policy providers plus guest arena allocation.

These crates are currently standalone because `manifold-wasm` is host/runtime-only. Once guest-side helpers exist, plugins can depend on a guest SDK instead of hand-writing ABI exports and JSON envelopes.
