# WSLg Implementation Workspace

This page tracks the WSLg-specific implementation path for Manifold.

## Thesis

The WSLg work should be a WSLg-native implementation, not a slow fallback of the generic nested compositor.

On WSLg, normal Linux GUI apps can use Mesa's D3D12 path and present through WSLg. The current nested Manifold prototype sits between the managed app and WSLg. Because WSLg does not expose DRM render nodes, GBM, importable DMA-BUF formats, or syncobj fences to Manifold's Winit/GLES presenter, GPU-capable hosted Wayland apps fall back to `wl_shm` when talking to Manifold.

The goal is to avoid this shape:

```text
managed app -> Manifold nested compositor -> SHM fallback -> Manifold presentation window -> WSLg -> Windows
```

and investigate this shape instead:

```text
managed app -> WSLg/Weston with Manifold session behavior -> WSLg accelerated presentation -> Windows
```

The WSLg-native path and the Linux nested compositor should remain separate experiments for now. The Linux path is Rust-first and has a presentation endpoint we control. The WSLg path is C-first inside Weston/WSLg and initially has no separate Manifold presentation endpoint; Windows windows are owned by WSLg/RDPRAIL. Unification should happen through a shared behavior spec before shared runtime code.

## Current Evidence

- Ubuntu 24.04 WSL and Arch WSL both expose `/dev/dxg` but no `/dev/dri/renderD*` in this environment.
- Manifold's DMA-BUF diagnostics report no importable DMA-BUF texture formats under WSLg.
- Direct WSLg clients can still use GPU acceleration through Mesa D3D12, which means the missing piece is not "WSL cannot accelerate GUI apps"; it is "Manifold's nested compositor cannot import those GPU buffers through normal Linux DMA-BUF APIs."
- Software Chrome nested through Manifold is surprisingly usable on UMA hardware, but it is still the wrong WSL architecture if we can keep clients on WSLg's native accelerated path.

## Investigation Targets

1. Build a private WSLg system distro from `microsoft/wslg`.
2. Locate the Weston shell/backend hooks that own xdg toplevel placement, frame callbacks, damage, and buffer lifecycle.
3. Prove a tiny private WSLg modification is actually loaded.
4. Prototype one Manifold behavior at the WSLg layer:
   - observe launched app surfaces;
   - detach/hide presentation without killing the app;
   - re-show the same app surface;
   - preserve input routing after re-show.
5. Compare latency and cadence against:
   - direct WSLg app;
   - current nested Manifold SHM path;
   - WSLg-native prototype.
6. Keep the dev environment reproducible enough for dev containers:
   - provide a one-liner Dockerfile or Docker image target for Manifold plus WSLg build dependencies;
   - provide an equivalent Nix expression if Nix becomes part of the workflow;
   - make it possible to enter a full workspace with Rust, WSLg build tools, Docker/buildx helpers, Go/tar2ext4, and the WSLg vendor bootstrap scripts available.

## Reference Checkout

The local upstream checkout should live at:

```text
reference/wslg
```

That directory is ignored by git along with the other reference repositories. The local experiment branch is:

```text
manifold-wslg-impl
```

Bootstrap or refresh it with:

```bash
./tools/wslg-bootstrap-reference.sh
```

Clone or refresh the WSLg vendor sources with:

```bash
./tools/wslg-bootstrap-vendors.sh
```

Prepare local build prerequisites and start a Docker daemon inside Ubuntu WSL with:

```bash
./tools/wslg-ensure-build-deps.sh
```

Build a private WSLg system distro VHD with:

```bash
./tools/wslg-build-private-system-distro.sh
```

Install the built VHD as the private WSLg system distro from PowerShell with:

```powershell
.\tools\wslg-install-private-system-distro.ps1 -Shutdown
```

After WSL restarts, verify the private system distro marker with:

```powershell
.\tools\wslg-verify-private-system-distro.ps1
```

Return to stock WSLg with:

```powershell
.\tools\wslg-use-stock-system-distro.ps1 -Shutdown
```

## Useful Upstream Paths

Expected WSLg sources after bootstrap:

```text
reference/wslg
reference/wslg/vendor/weston
reference/wslg/vendor/FreeRDP
reference/wslg/vendor/pulseaudio
reference/wslg/vendor/DirectX-Headers-1.0
reference/wslg/vendor/mesa
```

The first pass should focus on WSLg's documented private system distro flow before changing any Manifold runtime code.

## Local Bring-Up Status

- Ubuntu WSL has `docker.io` and `golang-go` installed for private WSLg builds.
- The Docker daemon is not managed by a normal WSL service in this distro, so `tools/wslg-ensure-build-deps.sh` starts `dockerd` manually when needed.
- Current user group membership may not include the `docker` group until a fresh login; build scripts use a temporary `docker` wrapper that calls `sudo docker`.
- `reference/wslg` is checked out on local branch `manifold-wslg-impl`.
- WSLg vendor sources have been bootstrapped under `reference/wslg/vendor`.
- PulseAudio's Microsoft mirror does not provide usable version metadata inside Docker because `.dockerignore` strips `.git`; the private build script writes `vendor/pulseaudio/.tarball-version` with `17.0`.
- A private WSLg VHD was successfully built at `reference/wslg/system_x64.vhd`.
- The private VHD was installed to `C:\Users\sargo\manifold-wslg\system_x64.vhd` through `.wslconfig` `systemDistro=...`.
- The active system distro has verified marker `manifold-wslg-impl` in `/etc/manifold-wslg.txt` and `/mnt/wslg/versions.txt`.
- `wayland-info` from Ubuntu connects to the modified WSLg instance and reports `weston_rdprail_shell`.

## Dev Container Goal

The WSLg experiment should eventually be reproducible from a one-liner development environment. The target shape is either:

```bash
docker run --rm -it -v "$PWD:/workspace" manifold/wslg-dev
```

or a Nix/dev-shell equivalent:

```bash
nix develop
```

The environment should include the normal Manifold Rust toolchain plus WSLg-specific build dependencies, enough Docker/buildx support for private system distro builds, Go support for `tar2ext4`, and scripts for bootstrapping WSLg vendor trees. The first version can be pragmatic and host-dependent; the important thing is that a future dev container can reproduce the full workspace setup without rediscovering the dependency chain.

## First Patch Marker

`tools/wslg-apply-manifold-marker.sh` applies a tiny Dockerfile patch to the ignored WSLg checkout. The patch writes `/etc/manifold-wslg.txt` into the private system distro and appends a marker to `/etc/versions.txt`.

This is intentionally small: it proves that a modified WSLg VHD is being built, installed, and run before we start changing Weston behavior.

## Questions To Answer

- Can a Weston shell/plugin give us enough control over toplevel lifecycle and visibility, or do we need to patch the WSLg Weston backend/shell directly?
- Where are frame callbacks sent relative to WSLg's final host presentation path?
- Can detach/reattach be represented as compositor visibility/state, or does WSLg's RAIL/VAIL integration require Windows-side window lifecycle changes?
- How does WSLg report and update monitor refresh, scale, and output geometry?
- Can Manifold policy stay in Rust over a side channel while WSLg keeps final presentation ownership?
