---
status: current
type: workflow
authority: implementation
code-links:
  - Cargo.toml
  - crates/broodd/Cargo.toml
  - tools/brood-build.sh
  - nix/packages.nix
---

# Building Brood

The default host build produces the full `brood` executable:

```sh
nix develop --command cargo build
```

For a fresh development build with every guest plugin, use the aggregate builder:

```sh
nix develop --command ./tools/brood-build.sh
./target/dev/bin/brood capabilities
```

Run Brood from the Nix development shell so its native libraries are available.
The builder checks Cargo freshness, validates newly encoded components, and
assembles the default client package. It also prepares `target/dev` with the
selected host executable, compatibility launchers, configuration, and links to
the checkout's guest artifacts. This directory follows the checkout; it is not
a portable installation. Re-running the builder refreshes stale inputs without
throwing away dependency caches. External editor runtimes and deployment bundles
are assembled by the Nix distribution, rather than by the guest compiler.

Guest workspaces share `target/guests`; published plugin paths remain unchanged.
The builder serializes guest Cargo invocations to share dependencies without
contending for target locks, while host compilation can run concurrently. See
[the builder](../../tools/brood-build.sh) for supported selections and controls.

For the specialized host build:

```sh
nix develop --command cargo build --no-default-features --features server
```

Cargo places both variants at the same executable path; the most recently built
variant wins. Rebuild with the default features to restore the full executable.
The executable's capability report identifies the selected variant. The server
omits the presentation client, WGPU, Winit, and text rendering; its compositor
still requires its own Wayland/EGL dependencies.

For an assembled installation, including the packaged guest and editor runtimes:

```sh
nix build
```

The explicit specialized executable is available with `nix build .#server`.
Default deployment bundles reuse the full executable. The explicit SSH and
container backend packages use the server variant. Package definitions live in
[packages.nix](../../nix/packages.nix).

Host linking uses the Nix-provided Mold linker. Development builds retain line
information for backtraces; the profiling profile retains full debug information.
Release artifacts strip debug information without changing runtime optimization.
See [Cargo.toml](../../Cargo.toml) for profile settings. The first build after
changing a profile or linker setting recompiles affected dependencies; measure
subsequent builds separately from that cache transition.
