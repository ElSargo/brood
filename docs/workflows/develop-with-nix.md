---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-07-03
code-links:
  - nix/scripts/dev-workflow.nix
  - nix/packages.nix
---

# Develop With Nix

Prefer this Nix-backed workflow for builds, checks, tests, and manual development so the toolchain and runtime libraries match CI.

The pasteable development workflow is:

```sh
nix run github:ElSargo/manifold#dev-workflow
```

It creates or reuses a writable checkout under `$XDG_CACHE_HOME/manifold/dev-workflow/source` or `~/.cache/manifold/dev-workflow/source`, installs packaged runtime files and WASM example artifacts, starts a normal Manifold session from the package-built binary, and opens a terminal in that checkout.

For the larger toolbox:

```sh
nix run github:ElSargo/manifold#dev-workflow-full
```

Use `dev-workflow` for host-visible manual app use. Use `nix develop --command ...` for repository-local build/test commands. Use [[test-nested]] for automated behavior validation.

Related:

- [[../systems/runtime]]
- [[../systems/configuration]]
- [[build-wasm-examples]]
