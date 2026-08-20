---
status: current
type: workflow
authority: product
last-reviewed: 2026-07-27
code-links:
  - crates/brood-remote/src/ssh.rs
  - crates/brood-remote/src/main.rs
  - tools/brood-ssh-backend-session.sh
  - nix/packages.nix
---

# Deploy Over SSH

From the repository root, deploy the current working tree to an SSH host and
open its presentation locally with one command:

```sh
nix develop --command cargo run -p brood-remote -- ssh sargo@luna
```

The packaged runtime's configured session init component is the package root by
default. The init component's directory is the package boundary. Its WASM
members are advertised by digest and transferred when the remote init actor
requests them.

The deploy command detects the target architecture, builds a portable backend
directory and complete local client runtime with the checked-in Nix flake,
copies the backend with `scp`, starts a private remote DBus session, backend,
and authenticated relay, then launches the packaged local `brood-client`. The
remote host does not build the project or import locally built paths into its
Nix store. The backend runs directly against the target's normal Linux ABI and
user namespace rather than a bundle sandbox, so normal host paths such as
`/bin/bash` remain visible.

The portable backend does not carry or execute Neovim. The local presentation
package carries the Wasm Neovim runtime and loads the local user's
configuration. The session package sent to the remote backend contains only
the bounded `neovim-files` actor; edits cross the authenticated actor-message
lane as revisioned file snapshots and atomic replacements.

QUIC uses UDP and therefore does not travel through OpenSSH's TCP forwarding.
The relay exposes one mutually authenticated UDP port. By default the local
client resolves the SSH hostname; use `--connect-host` to select a LAN address
while retaining the SSH host for deployment and lifecycle control.

Closing the local presentation stops the remote backend session. Verified
artifact content and deployment identity remain in per-host caches, so repeated
deployments reuse local builds and remote artifact state.

An already-running normal client exposes the same operation through
`New -> Connect remote backend`. Enter an OpenSSH target such as
`sargo@luna`; deployment runs in the background and the directory picker opens
when the new backend is authenticated and its daemon actor has returned the
remote directory catalogue. Typing filters real paths on the remote host; the
filesystem root is a fallback rather than the only synthetic choice. The
packaged client carries the portable backend needed by this flow, so it does
not require a source checkout or a remote build.

For an offscreen acceptance proof of the same packaged workflow, including the
ordinary local backend, run:

```sh
nix develop --command cargo run -- test ssh sargo@luna
```

The retained proof contains the combined presentation, distinct backend actor
realms, their client frontends, and clean remote-session shutdown. A target
other than Luna can be supplied without changing the harness.

Exact command options and parser behavior are defined by
`crates/brood-remote/src/ssh.rs`.

Related:

- [[../targets/systems/artifact-distribution]]
- [[../targets/systems/presentation-transport]]
- [[develop-with-nix]]
