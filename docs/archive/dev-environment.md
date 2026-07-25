# Development Environment

This archived page records earlier environment planning. Current checked-in workflows prefer the Nix flake for builds, checks, and behavior tests.

Brood should be easy to enter from a fresh machine or dev container without vendoring every Rust dependency.

## Current Shape

- Rust dependencies resolve through normal Cargo metadata.
- `target/`, `vendor/`, `.cargo/`, and `reference/` are local workspace artifacts and are ignored.
- `flake.nix` is the preferred checked-in build and test environment.

## Dev Container Goal

The target is a one-liner workspace for future development containers:

```bash
docker run --rm -it -v "$PWD:/workspace" brood/dev
```

or the Nix shell:

```bash
nix develop
```

The environment should include:

- Rust and the native libraries needed by `brood`
- Wayland, EGL/GLES, Smithay, and smoke-test dependencies
- Docker/buildx or an equivalent image-build path when container workflows need it

The first version can be pragmatic and host-dependent. The important goal is that a future contributor can reproduce the compositor workspace without rediscovering the dependency chain.

## Nix Flake Shape

The stock flake output should stay app-sized by default:

```bash
nix run github:ElSargo/brood
```

That path resolves to the packaged `brood` app. It is the output to cache
and download when the goal is just to run Brood.

The development-oriented outputs are explicit:

```bash
nix run github:ElSargo/brood#dev-workflow
nix run github:ElSargo/brood#dev-workflow-full
nix develop github:ElSargo/brood#full
```

Use `dev-workflow` for the smallest hosted app workflow. Use
`dev-workflow-full` or `nix develop .#full` when Docker, Go, Firefox, and smoke
tools are part of the work.

The top-level `flake.nix` only routes systems and imports. Reusable profiles,
package definitions, and workflow scripts live under `nix/`.

Downstream flakes can compose Brood into a personalized environment by using
the exposed dev shell or package bundle:

```nix
{
  devShells.${system}.default = pkgs.mkShell {
    inputsFrom = [ brood.devShells.${system}.full ];
    packages = [
      brood.packages.${system}.dev-env-full
      # personal tools go here
    ];
  };
}
```

## Nix Run Developer Workflow

The target Nix workflow is a pasteable command that creates an editable checkout
and starts a normal Brood development session:

```bash
nix run github:ElSargo/brood#dev-workflow
```

The app-sized workflow package clones the Brood repository into
`$XDG_CACHE_HOME/brood/dev-workflow/source` or
`~/.cache/brood/dev-workflow/source`, starts a normal `brood run` session
from that checkout, and opens a terminal in the checkout through Brood's
normal hosted-app path.

Before launching Brood, the workflow installs the packaged `wasm-examples`
artifacts into the checkout paths that the checked-in `brood.toml` refers to.
The WASM build is therefore cacheable through Nix/Cachix and the app workflow no
longer needs to download a Rust toolchain just to start.

The package-built binary comes from the Crane package path in `nix/packages.nix`.
The Brood process deliberately bypasses configured session runners because
the Nix package already provides the runtime for this entry point.

CI currently treats cached Nix check outputs as authoritative when they are
available from the trusted Cachix cache. This is the normal Nix model: if the
derivation and inputs match, substituting `checks.test` is equivalent to a prior
successful run by a trusted builder. We may still choose to add an explicit
GitHub-only test lane later if we want every CI run to execute the test binaries
again instead of accepting the cached check output.

The cached checkout is intentionally writable and reused between runs. The
workflow should not reset local edits in that checkout. To point the one-liner at
another repository, branch, checkout directory, or startup client, set
`BROOD_DEV_REPO_URL`, `BROOD_DEV_REF`, `BROOD_DEV_SOURCE_DIR`, or
`BROOD_DEV_CLIENT` before running the package.

This workflow is for host-visible manual app use. Automated graphical behavior
tests should still use the built-in nested test CLI; for exact `brood test
nested` arguments, see `crates/broodd/src/main.rs`.
