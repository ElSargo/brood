# Tools

Checked-in development and verification tools live here. Run them through the
repository's Nix environment.

## Builds

`brood-build.sh` builds the host programs and every WASM module with one shared
parallel jobserver. Cargo checks each independent workspace for staleness, and
Component Model artifacts are encoded and validated only after their core
module changes. Once the independent builds finish, the tool co-locates the
default client's declared package members with the session-switcher root so
the repository `brood.toml` is directly runnable:

```sh
nix develop --command ./tools/brood-build.sh
```

Run `./tools/brood-build.sh --help` for the build modes and concurrency control.

## Presentation Proofs

`test-cua-divider-resize.sh` launches the packaged production server, terminal,
session-switcher, and client under private headless Sway, then uses the external
Cua project driver to perform native Wayland divider drags. It captures the
visible divider during each gesture and grades tracking, synchronized-layout
efficiency, final correctness, survival, and release latency. Every presented
frame is also checked for exact rendered-surface, pane, and accepted-layout
area and geometry, while native packet extent is measured separately from its
projected output. Captured pixels are independently scanned for renderer-clear
slices near pane boundaries and screen edges. An absent sample, mismatch, or
clear slice is a hard failure. The default gate uses a verified 180 Hz headless
output, requests one Cua motion step per target frame, and reports the worst of
three alternating drags. Native-content
projection, forward/regressive relayouts, and partial-cohort presentations are
reported separately; a backward relayout is a hard failure. A presentation-rate
element oracle also tracks the session switcher's fixed-width new-session
button. It projects that button from the native packet coordinate space through
the actual pane stretch on every successful presentation and fails if its drawn
position or size differs from current target geometry by more than 0.25 px:

```sh
nix develop --command bash tools/test-cua-divider-resize.sh
```

Set `CUA_DRIVER_BIN` when `cua-driver` is not on `PATH`. During investigation,
`BROOD_CUA_ALLOW_FAIL=1` retains a failing grade without making the shell
command fail. `BROOD_CUA_RESIZE_OUT` selects the retained artifact directory.
`BROOD_CUA_TARGET_FRAME_RATE_HZ` overrides the default 180 Hz target; unless
`BROOD_CUA_DRAG_STEPS` is explicit, the gesture step count follows that rate.
Interactive divider resizing uses genuine live pane/PTY relayout by default.
Set `BROOD_DIVIDER_RESIZE_MODE=stable-projection` to use the optional mode that
holds one native pane image through the gesture and performs an exact redraw on
release; `native` selects the default explicitly.

The Cua daemon uses a private socket and disposable nested display, so the proof
does not stop or reuse an operator's existing Cua daemon.

The same capture records a correlated resize pipeline for the session switcher
and terminal. `metrics.json` and `report.md` break the exact first presentation
for each actor event watermark into admission-to-dispatch, guest draw, host
validation, graphics readiness, composition wait, GPU submission, and present
stages. Preparation queue, worker, and completion-hop distributions are also
reported by pane/scene sequence.

## System profiling

`profile-brood.sh` enables bounded cross-process profiling for a supplied real
workload and produces both a structured report and a Chrome/Perfetto trace.
Build binaries with Cargo's `profiling` profile before capture so native stack
sampling and semantic events describe optimized code. See
[[../docs/targets/systems/system-profiler]] for the measurement contract.

Run the script without arguments for its exact invocation.

`test-client-layout-actor.sh` starts a real `brood-client` under headless
Weston, maps real managed applications, and verifies client-owned layout,
managed-pane size convergence, and the presentation key-policy boundary.

`test-session-switcher-plugin-nested.sh` exercises the default client init
plugin and recursive layout chain. It starts real windows in two sessions
before the client connects, verifies that both enter the client catalog and
layout lease while only the selected session is shown by default, and captures
the visible result on switch. Catalog revisions must arrive through the
dedicated shared-memory lane rather than a DBus polling worker. It also drives
real keys, clicks, and text; opens the reusable fuzzy picker as a floating
pane; selects a session through a non-contiguous query;
creates, renames, and closes sessions; hides and restores the right-hand
sidebar through an ordinary actor action; and retains graphical proof
artifacts.

`brood test editor --out-dir DIR` builds the packaged editor integration and
runs it on a private session bus with an external client under headless Sway.
The proof opens the editor through the real Ctrl+Alt+E binding, confirms that
the local Wasm runtime loaded a host Neovim config marker, edits and remotely
writes a fixture with modal Neovim input, opens another file through an actor
message, round-trips a file spanning more than 64 runner chunks, and exercises
editor-to-diff-terminal, editor-to-terminal, and editor-to-agent routes. It
retains screenshots, logs, written fixtures, and structured metrics in `DIR`.

`software-e2e-capture.sh` runs the external WGPU client with real Wayland
workloads and retains initial and reconnected client frames:

```sh
nix run .#software-e2e-capture -- --out-dir /tmp/brood-software-e2e
```

`brood test ssh USER@HOST` builds and deploys the current source to an SSH
host, then starts one offscreen presentation connected to both that remote
backend and an ordinary local backend. The proof requires realm-qualified
actor traffic and a default terminal frontend from both backends, captures the
combined presentation, and verifies that closing it also stops the remote
session. Use `--connect-host` when QUIC should use a different address from the
SSH destination.

These are the retained acceptance paths because they exercise current,
externally visible behavior through a real presentation client. Presentation
proofs use frontend-owned layout state, real input, and client capture. Server
diagnostics may accompany a proof, but are not acceptance evidence for final
layout, hit testing, or pixels.

## Complete Plugin Matrix

`test-bundled-plugins.sh` audits `bundled-plugin-e2e.tsv` against every bundled
plugin crate and runs the referenced focused lanes. The matrix records the
synthetic input and the screen, disk, process, protocol, or structured-runtime
state that decides acceptance. Run it through `nix develop`; use `--all` for
the complete suite or `--lane NAME` while iterating.

`test-runtime-plugins-nested.sh` is reached through the built-in nested test
supervisor. It is intentionally disk-authoritative: Component runtime I/O,
Component filesystem watch, and Preview 1 filesystem/runtime behavior leave
human-readable workspace artifacts and bounded resource snapshots rather than
inventing a graphical proxy for server-side plugins.
