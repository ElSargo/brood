---
status: implemented
type: progress
last-reviewed: 2026-09-04
---

# GPUI Presentation Transition

The product `brood-client` now enters and remains in GPUI's application loop.
The former Winit loop, WGPU composition modules, Wayland drop bridge, and
duplicate client renderer were removed. `brood-gpui` is the single adapter
crate allowed to depend on GPUI.

## Implemented boundaries

- The versioned managed-UI protocol has bounded snapshots, atomic patches,
  stable actor-generation-scoped node identity, state modes, actions,
  capabilities, accessibility semantics, and structured diagnostics.
- GPUI renders the semantic vocabulary and owns transient interaction, native
  text fields, IME, clipboard, focus, pointer routing, drag/drop, and Linux
  AT-SPI projection through AccessKit.
- Status bar, session switcher, picker, inbox, and terminal shell publish
  semantic documents through only the public SDK/protocol. Terminal cells keep
  the canvas escape hatch.
- Actor lifecycle, capability validation, local/session/daemon messaging,
  backend catalogue, session commands, layout leases, focus, and exact
  allocated pane resize events are hosted alongside GPUI without foreground
  actor I/O.
- Canvas packets share the GPUI presentation tree. Text resources use GPUI's
  real font shaping, wrapping, baselines, and monospace cell metrics.
- The local frame path imports sessiond DMA-BUFs into Blade/Vulkan with explicit
  format modifier, stride, offset, source crop, inversion, submission lifetime,
  and descriptor validation. It performs no CPU readback.
- GPUI 0.2.2 and Blade Graphics 0.7.1 are exact, audited local forks. Their
  provenance and maintenance boundary are in `third_party/README.md`.
- The same-instance pure-render export is wired through the Component Model,
  carries dirty, geometry, deadline, and reduced-event revisions, and commits
  only current complete pane output. Render-phase capability masking is scoped
  to the render future, so an unrelated worker suspended in a host call cannot
  either inherit the mask or starve a due render.
- Output pacing uses the active Wayland mode's refresh rate rather than a
  hard-coded 60 Hz interval. Late pure results are discarded against the
  monotonic submission cutoff and leave retained content visible.
- Canvas document insertion and its packet replacement now share one bounded
  presentation command. Resize candidates validate staged canvas documents in
  a cloned document store before atomically promoting document, packet, and
  geometry state.
- The remote backend connection and SSH/container deployment paths were moved
  into the GPUI client rather than retained through the removed Winit client.
- Root keyboard, pointer, scrolling, and cursor behavior is installed through
  GPUI element handlers during paint. Debug builds therefore enforce the same
  lifecycle as release builds instead of relying on disabled paint assertions.
- Layout inventory revision and presented layout revision are distinct. Child
  actor startup republishes inventory, redundant legacy fragments do not create
  fictitious commits, and synchronized layouts base themselves on the actual
  committed geometry. Declarative managed panes are immediately ready for an
  atomic geometry reflow while retained canvas panes still stage exact-epoch
  content.

## Verification record

- `cargo check --workspace --all-targets` passed in the checked-in Nix shell.
- All independent bundled plugin workspaces passed locked `wasm32-wasip2`
  checks after the SDK protocol dependency changed.
- The complete workspace test suite passed; the one systemd/rootless-namespace
  host smoke test remained intentionally ignored by its existing contract.
- Workspace Clippy, including the GPUI fork consumers, passed with warnings
  denied.
- The GPUI adapter has regression coverage for atomic rejection of invalid
  actor-frame batches, actor/pane resource isolation, affine packet validation,
  recursive layout composition, and managed-document generation replacement.
- Terminal pure rendering retains bounded layout resources and coalesced
  metrics, and cursor blinking retains at most one timer worker during large
  presentation input batches.
- The focused nested inbox workload passed with real pointer input and
  Ctrl+Alt+I, an accepted synchronized four-pane layout, `pane_ready=4/4`, and
  a non-degraded atomic commit. Its retained proof is
  `/tmp/brood-terminal-inbox-gpui-packaged-final/proof.md`.
- A complete Nix package build passed at
  `/nix/store/s2s9y14gm5izcmy7xfyrg52akkls8a24-brood`.

The nested workload initially stopped at a missing `tmux`; the smoke-tool Nix
profile now includes it. A subsequent package proof exposed two Crane-specific
issues: patched path dependencies must retain their real API in the dummy
dependency source, and GPUI's WGSL build input must survive member filtering.
Both are now encoded in `nix/packages.nix` rather than relying on a developer
checkout accident.
