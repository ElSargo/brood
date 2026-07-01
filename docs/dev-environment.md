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

- Rust and the native libraries needed by `manifold`
- Wayland, EGL/GLES, Smithay, and smoke-test dependencies
- Docker/buildx or an equivalent image-build path when container workflows need it

The first version can be pragmatic and host-dependent. The important goal is that a future contributor can reproduce the compositor workspace without rediscovering the dependency chain.

## Nix Flake Shape

The stock flake output should stay app-sized by default:

```bash
nix run github:ElSargo/manifold
```

That path resolves to the packaged `manifold` app. It is the output to cache
and download when the goal is just to run Manifold.

The development-oriented outputs are explicit:

```bash
nix run github:ElSargo/manifold#dev-workflow
nix run github:ElSargo/manifold#dev-workflow-full
nix develop github:ElSargo/manifold#full
```

Use `dev-workflow` for the smallest hosted app workflow. Use
`dev-workflow-full` or `nix develop .#full` when Docker, Go, Firefox, and smoke
tools are part of the work.

The top-level `flake.nix` only routes systems and imports. Reusable profiles,
package definitions, and workflow scripts live under `nix/`.

Downstream flakes can compose Manifold into a personalized environment by using
the exposed dev shell or package bundle:

```nix
{
  devShells.${system}.default = pkgs.mkShell {
    inputsFrom = [ manifold.devShells.${system}.full ];
    packages = [
      manifold.packages.${system}.dev-env-full
      # personal tools go here
    ];
  };
}
```

## Nix Run Developer Workflow

The target Nix workflow is a pasteable command that creates an editable checkout
and starts a normal Manifold development session:

```bash
nix run github:ElSargo/manifold#dev-workflow
```

The app-sized workflow package clones the Manifold repository into
`$XDG_CACHE_HOME/manifold/dev-workflow/source` or
`~/.cache/manifold/dev-workflow/source`, starts a normal `manifold run` session
from that checkout, and opens a terminal in the checkout through Manifold's
normal hosted-app path.

Before launching Manifold, the workflow installs the packaged `wasm-examples`
artifacts into the checkout paths that the checked-in `manifold.toml` refers to.
The WASM build is therefore cacheable through Nix/Cachix and the app workflow no
longer needs to download a Rust toolchain just to start.

The package-built binary comes from the Crane package path in `nix/packages.nix`.
The Manifold process deliberately bypasses configured session runners because
the Nix package already provides the runtime for this entry point.

The cached checkout is intentionally writable and reused between runs. The
workflow should not reset local edits in that checkout. To point the one-liner at
another repository, branch, checkout directory, or startup client, set
`MANIFOLD_DEV_REPO_URL`, `MANIFOLD_DEV_REF`, `MANIFOLD_DEV_SOURCE_DIR`, or
`MANIFOLD_DEV_CLIENT` before running the package.

This workflow is for host-visible manual app use. Automated graphical behavior
tests should still use the built-in nested test CLI; for exact `manifold test
nested` arguments, see `crates/manifoldd/src/main.rs`.
