---
status: current
type: workflow
authority: product
last-reviewed: 2026-08-01
code-links:
  - crates/brood-remote/src/container.rs
  - clients/brood-client/src/main.rs
  - plugins/session-switcher/src/lib.rs
  - nix/packages.nix
---

# Container Backends

A container backend is an ordinary backend-qualified Brood session realm whose
filesystem, processes, devices, and working directories come from an OCI
container. It uses the same authenticated transport, artifact hydration,
catalogue, actor routes, and presentation layout as local and SSH backends.

The container deployer works with the local Podman engine or a named Podman
system connection. It copies a self-contained backend runtime into the
container rather than requiring Brood, Nix, glibc, or development tools in the
selected image. The owning client keeps the container deployment alive and
removes it when the backend is disconnected unless retention was requested.

Start a container backend and open it alongside the ordinary local backend:

```sh
nix shell .#brood -c brood-remote container docker.io/library/alpine:latest
```

The packaged command is implemented by `brood-remote`; exact image, Podman
connection, bind-mount, working-directory, GPU, retention, and client options
are defined by `ContainerArgs` in `crates/brood-remote/src/container.rs`.

In the session switcher, choose `New`, then either `Create container backend`
or `Create remote container backend`. The local flow accepts an OCI image. The
remote flow accepts a named Podman system connection and an image separated by
`::`. Once the relay is authenticated, the normal backend directory picker
creates a session inside that container.

The `podman` flake package is a drop-in command frontend. Non-`run` operations
are executed by the real Podman binary unchanged. `run` retains the requested
image, command, volumes, networking, interactivity, detachment, and other
Podman arguments while installing and starting the Brood backend alongside the
container workload. GPU forwarding is attempted automatically and retried
without GPU devices when the engine cannot provide them. An explicit Podman
GPU option remains authoritative; `BROOD_CONTAINER_GPU=never` disables the
automatic attempt.

```sh
nix shell .#podman
podman run --rm -it docker.io/library/alpine:latest /bin/sh
```

The backend runtime includes the EGL/Mesa software path as a portable fallback.
When Podman can expose a GPU through its normal `--gpus all` capability, the
same runtime can use the forwarded render device. Container authority remains
the selected image, mounts, user, network, and device policy; Brood does not
silently bridge host paths or credentials into the container.

Related:

- [[ssh-deploy]]
- [[../decisions/0008-multi-backend-presentation]]
- [[../concepts/session]]
