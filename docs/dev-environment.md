# Development Environment

Manifold should be easy to enter from a fresh machine or dev container without vendoring every Rust dependency.

## Current Shape

- Rust dependencies resolve through normal Cargo metadata.
- `target/`, `vendor/`, `.cargo/`, and `reference/` are local workspace artifacts and are ignored.
- `flake.nix` is available for developers who want a Nix shell, but it is not the required entry point.

## Dev Container Goal

The target is a one-liner workspace for future development containers:

```bash
docker run --rm -it -v "$PWD:/workspace" manifold/dev
```

or the optional Nix shell:

```bash
nix develop
```

The environment should include:

- Rust and the native libraries needed by `manifoldd`
- Wayland, EGL/GLES, Smithay, and smoke-test dependencies
- Docker/buildx or an equivalent image-build path when container workflows need it

The first version can be pragmatic and host-dependent. The important goal is that a future contributor can reproduce the compositor workspace without rediscovering the dependency chain.

## Nix Run Developer Workflow

The target Nix workflow is a pasteable command that creates an editable checkout
and starts a normal Manifold development session:

```bash
nix run github:ElSargo/manifold#dev-workflow
```

The package clones the Manifold repository into
`$XDG_CACHE_HOME/manifold/dev-workflow/source` or
`~/.cache/manifold/dev-workflow/source`, starts a normal `manifoldd run` session
from that checkout, and opens a terminal in the checkout through Manifold's
normal hosted-app path.

Before launching Manifold, the workflow builds every `examples/wasm-*` crate for
`wasm32-unknown-unknown` in release mode. This produces the WASM artifacts that
the checked-in `manifold.toml` refers to, so the one-liner starts with the same
module/startup config as a normal local Manifold run.

The package-built binary comes from the checked-in crate2nix expression
generated from `Cargo.lock`. The Manifold process deliberately bypasses
configured session runners because the Nix package already provides the runtime
and development tools for this entry point.

The cached checkout is intentionally writable and reused between runs. The
workflow should not reset local edits in that checkout. To point the one-liner at
another repository, branch, checkout directory, or startup client, set
`MANIFOLD_DEV_REPO_URL`, `MANIFOLD_DEV_REF`, `MANIFOLD_DEV_SOURCE_DIR`, or
`MANIFOLD_DEV_CLIENT` before running the package.

This workflow is for host-visible manual development. Automated graphical
behavior tests should still use the built-in nested test CLI; for exact
`manifoldd test nested` arguments, see `crates/manifoldd/src/main.rs`.
