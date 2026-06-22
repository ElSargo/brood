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
