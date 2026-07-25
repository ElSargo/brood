# Session Runner Nix Project

This is a small project for testing Brood session runners. Its `flake.nix`
sets marker variables that should be visible to anything launched by
`brood-sessiond` when the `nix-develop` runner is active.

Build the Brood binaries from the repo root first:

```sh
nix develop --command cargo build -p broodd --bins
```

Quick runner-only smoke:

```sh
cd examples/session-runner-nix-project
../../target/debug/brood enter ./scripts/check-env.sh
```

Full `brood enter` smoke:

```sh
cd examples/session-runner-nix-project
../../target/debug/brood enter ./scripts/check-env.sh
../../target/debug/brood enter ../../target/debug/brood ctl quit
```

The Nix project reuses this repository's dev shell through a local flake input
and then adds marker variables. On a cold machine, the first `nix develop` may
still wait on Nix cache or input availability before Brood can become ready.
