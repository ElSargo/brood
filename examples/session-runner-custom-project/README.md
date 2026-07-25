# Session Runner Custom Project

This project tests the `brood enter` workflow without depending on Nix downloads.
It uses the same runner ABI as the Nix runner:

```text
runner <project-dir> <working-dir> <session-cache-dir> <runner-cache-root> <inner-binary>
```

The runner exports marker variables, runs the supplied inner binary, and exits
with the inner binary's status. The supplied inner binary resolves the assigned
program or session arguments from its current cgroup. Brood starts the
runner from the directory where `brood enter` was invoked.

Build Brood first:

```sh
nix develop --command cargo build -p broodd --bins
```

Then test the intended workflow:

```sh
nix develop --command bash -lc '
  cd examples/session-runner-custom-project
  ../../target/debug/brood enter ./scripts/check-env.sh
  ../../target/debug/brood enter ../../target/debug/brood ctl quit
'
```

The surrounding repo dev shell supplies Brood's native runtime libraries for
this intentionally tiny runner. Real environment runners should provide the
libraries and tools their session server needs.
