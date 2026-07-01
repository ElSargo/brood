---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-07-01
---

# Run Manifold

From the repository root:

```sh
cargo run
```

This runs the default `manifold` binary and starts the default runtime path. By default it loads config modules and startup apps, starts the nested Wayland compositor, exposes DBus control, and attaches presentation.

Useful variants:

```sh
cargo run -- check
cargo run -- run --headless
cargo run -- run --no-default-modules
cargo run -- run --config ./manifold.toml
cargo run -- ctl status-json
```

Use `run --headless` for service-style runtime sessions. Use [[test-nested]] for automated behavior validation.

Related:

- [[../systems/runtime]]
- [[../systems/configuration]]
