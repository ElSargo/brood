---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-07-03
---

# Run Manifold

From the repository root, prefer the Nix development shell:

```sh
nix develop --command cargo run
```

This runs the default `manifold` binary and starts the default runtime path. By default it loads config modules and startup apps, starts the nested Wayland compositor, exposes DBus control, and attaches presentation.

Useful variants:

```sh
nix develop --command cargo run -- check
nix develop --command cargo run -- run --headless
nix develop --command cargo run -- run --no-default-modules
nix develop --command cargo run -- run --config ./manifold.toml
nix develop --command cargo run -- ctl status-json
```

Use `run --headless` for service-style runtime sessions. Use [[test-nested]] for automated behavior validation.

Related:

- [[../systems/runtime]]
- [[../systems/configuration]]
