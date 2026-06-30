# Session Runner Nix Project

This is a small project for testing Manifold session runners. Its `flake.nix`
sets marker variables that should be visible to anything launched by
`manifold-sessiond` when the `nix-develop` runner is active.

Build the Manifold binaries from the repo root first:

```sh
nix develop --command cargo build -p manifoldd --bins
```

Quick runner-only smoke:

```sh
cd examples/session-runner-nix-project
../../target/debug/manifold enter ./scripts/check-env.sh
```

Full `manifold enter` smoke:

```sh
cd examples/session-runner-nix-project
../../target/debug/manifold enter ./scripts/check-env.sh
../../target/debug/manifold enter ../../target/debug/manifold ctl quit
```

The Nix project reuses this repository's dev shell through a local flake input
and then adds marker variables. On a cold machine, the first `nix develop` may
still wait on Nix cache or input availability before Manifold can become ready.
