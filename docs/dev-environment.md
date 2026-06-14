# Development Environment

Manifold should be easy to enter from a fresh machine or dev container, but the first commit should not vendor every Rust dependency or WSLg source checkout.

## Current Shape

- Rust dependencies resolve through normal Cargo metadata.
- `target/`, `vendor/`, `.cargo/`, and `reference/` are local workspace artifacts and are ignored.
- WSLg source checkouts live under `reference/wslg` when needed.
- WSLg changes are tracked in Manifold as scripts and patch files under `tools/`.

## Dev Container Goal

The target is a one-liner workspace for future development containers:

```bash
docker run --rm -it -v "$PWD:/workspace" manifold/wslg-dev
```

or a Nix equivalent:

```bash
nix develop
```

The environment should include:

- Rust and the native libraries needed by `manifoldd`
- Wayland, EGL/GLES, Smithay, and smoke-test dependencies
- WSLg build dependencies for private system distro experiments
- Docker/buildx or an equivalent image-build path
- Go support for `tar2ext4`
- scripts for bootstrapping WSLg vendor sources

The first version can be pragmatic and host-dependent. The important goal is that a future contributor can reproduce the full compositor and WSLg workspace without rediscovering the dependency chain.
