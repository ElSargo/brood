# WSLg Implementation Workspace

This page tracks the WSLg-specific implementation path for Manifold.

This page is one of the explicit exceptions to the usual doc default: here, unqualified implementation language means the Windows/WSLg-native path, not the Linux-native path.

## Thesis

The WSLg work should be a WSLg-native implementation, not a slow fallback of the generic nested compositor.

The product goal is not "remote desktop." The goal is to keep managed app surfaces on WSLg's accelerated GPU-backed presentation path, preserve Manifold workspace behavior at the compositor/session layer, and minimize added presentation latency.

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
- Software Chrome nested through Manifold is surprisingly usable on UMA hardware, but it is still the wrong WSL architecture if we can keep clients on WSLg's native accelerated path and avoid copy-heavy surface movement.

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

### Fast Iteration Deploy

For a reliable one-command redeploy of the WSLg path and the two-Chrome
composite smoke setup, run:

```powershell
.\tools\wslg-redeploy-two-chrome.ps1
```

That rebuilds the private WSLg system distro VHD, installs it through
`.wslconfig`, restarts WSL, opens isolated red/green Chrome browser windows in the
target distro, and verifies the private system distro marker. Use
`-SkipBuild -SkipInstall` to reopen the two Chrome test windows against the
currently active WSLg image.

For Weston/WSLg iteration, the experimental hot-deploy loop can rebuild just the
Weston artifacts:

```powershell
.\tools\wslg-hot-deploy-weston.ps1 -WestonOnly
```

That command uses a cached WSLg dev-stage image as the build environment,
rebuilds Weston incrementally from the local `reference/wslg/vendor/weston`
checkout, and copies the hot-deploy artifacts into the active private system
distro. It skips rebuilding the final runtime image, Docker export, tar-to-VHD
conversion, VHD copy, and `.wslconfig` rewrites.

Current limitation: this path is useful for producing artifacts, but it is not
yet a reliable end-to-end redeploy. The active system distro is an overlay over
the private system VHD. Copying binaries into the running system distro updates
the overlay and checksums correctly, but `wsl --shutdown` discards that overlay.
The next GUI launch therefore reloads the binaries from the private VHD, not the
hot-copied files. Use the full private VHD build/install path when the next
verification needs a clean WSLg restart.

Use the full-image hot deploy when you need the final runtime image rebuilt as well:

```powershell
.\tools\wslg-hot-deploy-weston.ps1
```

If the Docker image is already current, skip the rebuild and only copy the binaries:

```powershell
.\tools\wslg-hot-deploy-weston.ps1 -WestonOnly -SkipBuild
```

Use `-NoShutdown` to copy binaries without immediately restarting WSL, and
`-DryRun` to list the files that would be deployed.

Hot deploy is intended for changes to Weston, the RDP backend, RDPRAIL shell,
WSLGd, and renderer modules, but the reload step still needs work. Use the full
VHD deploy when changing the base system image, installed packages, startup
configuration, Dockerfile-only system contents, when producing a reproducible
artifact, or when the test must survive `wsl --shutdown`.

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
- Private VHD installs use timestamped files under `C:\Users\sargo\manifold-wslg\` through `.wslconfig` `systemDistro=...`, so a running system distro does not block the next copy.
- The active system distro has verified marker `manifold-wslg-fastpath-required` in `/etc/manifold-wslg.txt` and `/mnt/wslg/versions.txt`.
- `wayland-info` from Ubuntu connects to the modified WSLg instance and reports `weston_rdprail_shell`.
- The GL compositor negative-effect shader was removed from the local Weston
  source and from the Manifold patch application flow. Private WSLg rebuilds no
  longer apply `tools/patches/wslg-invert-shader-poc.patch`, and the Pixman
  inversion hook is now opt-in through `WSLG_APPLY_PIXMAN_INVERT=1`.
- A two-Chrome validation with red and green app windows proved the current
  hot-deploy reload problem. After copying clean non-inverting renderer
  artifacts into the live system distro and then restarting WSL, the captured
  composite window was still inverted:
  `left_red=0/9600`, `left_cyan=8733/9600`, `right_green=0/9600`,
  `right_magenta=8703/9600`.
- The live overlay copy itself did work: checksums for
  `/usr/lib/libweston-9/gl-renderer.so` and
  `/usr/lib/libweston-9.so.0.0.0` matched the local artifacts, and the inversion
  shader strings were absent on disk. The running Weston process still had old
  mapped objects shown as `(deleted)` in `/proc/<weston-pid>/maps`, so it kept
  rendering with the previously loaded negative effect.
- Killing only Weston caused WSLGd to respawn Weston against the current overlay
  files, and the new process maps no longer showed `(deleted)` libraries. That
  did not restore a healthy Windows RAIL export for the Chrome test: Weston saw
  the Chrome surfaces, but no visible `RAIL_WINDOW` host window appeared on the
  Windows side.
- A direct private-VHD patch route was investigated but not adopted. The active
  private system VHD is an ext4 image with the `read-only` filesystem feature set
  and essentially no free space. It can be copied and mounted with `wsl --mount`,
  but safe writable patching needs a deliberate VHD resize/feature workflow. For
  now, prefer rebuilding and reinstalling the private VHD instead of editing it
  in place.

## Fast-Path Requirement

The private WSLg experiment must fail loudly if the RDP backend cannot use graphics redirection shared memory. The stock upstream path treats `WESTON_RDP_SHARED_MEMORY=true` as a preference: it probes `WSL2_SHARED_MEMORY_MOUNT_POINT`, FreeRDP gfxredir support, and a tiny `/mnt/shared_memory` allocation, then silently falls back to copy-heavy RDPGFX surface commands when any probe fails. It also allows the slow path when `WESTON_RDP_SHARED_MEMORY=false`; Manifold does not.

Manifold patches this into a hard startup/runtime failure with `tools/patches/wslg-require-gfxredir-fastpath.patch`. `tools/patches/wslg-retry-gfxredir-shm-open.patch` also gives the real gfxredir shared-memory allocation a bounded wait for the Windows-side namespace to become usable. A Weston log line with `refusing slow RDPGFX fallback` is therefore intentional: fix the shared-memory path before benchmarking or using fullscreen app workloads.

## Two-Client Composite PoC

`tools/patches/wslg-composite-two-clients-poc.patch` adds the first WSLg compositor-side composition experiment. The first normal RAIL toplevel becomes a two-cell composite host window; the next normal toplevel is kept compositor-local with no Windows/RAIL window of its own, both surfaces are server-resized into adjacent borderless cells, and shell window geometry is used when copying so compositor-managed borders are skipped. Pointer input is translated from the exported presentation surface into the matching client cell before Weston dispatch. This is intentionally scoped to the triangle/cube and two-Chrome proof: it is enabled by default in the private WSLg build and can be disabled with `MANIFOLD_WSLG_COMPOSITE=0`.

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
