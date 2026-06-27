# Session Runner Custom Project

This project tests the `mctl enter` workflow without depending on Nix downloads.
It uses the same runner ABI as the Nix runner:

```text
runner
```

The runner exports marker variables and then execs `manifold-runner-exec`,
which resolves the assigned program from its current cgroup.

Build Manifold first:

```sh
nix develop --command cargo build -p manifoldd --bins
```

Then test the intended workflow:

```sh
nix develop --command bash -lc '
  cd examples/session-runner-custom-project
  ../../target/debug/mctl enter ./scripts/check-env.sh
  ../../target/debug/mctl enter ../../target/debug/mctl ctl quit
'
```

The surrounding repo dev shell supplies Manifold's native runtime libraries for
this intentionally tiny runner. Real environment runners should provide the
libraries and tools their session server needs.
