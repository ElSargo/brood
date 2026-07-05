---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-03
code-links:
  - crates/manifoldd/src/compositor.rs
  - tools/audit-v1-capabilities.sh
  - tools/smoke-v1-full.sh
---

# Testing

The supported behavior-test path is the built-in `manifold test` CLI, run through the Nix development shell for checked-in workflows. It owns nested topology policy and avoids creating a host OS window by default.

Use product runtime commands such as `manifold run` for runtime/manual use, not automated behavior validation.

## Built-In Nested Tests

`manifold test` defaults to `manifold test nested`.

Outside Manifold, the default topology starts an outer supervisor with offscreen presentation, then starts the subject compositor inside it. Inside an existing Manifold session, the test command detects `manifold_session` and starts only the subject compositor unless `--force-outer` is used.

The outer supervisor is offscreen-attached unless `--outer-headful` is passed.

Nested tests render surface-local fiducial markers in hosted surfaces before final composition. Debug dumps include the composed framebuffer and matching marker IDs in `state.json`, so image-based checks can verify that the captured pixels and Manifold's expected pane/layout metadata agree.

The sandboxed graphical E2E path is `checks.software-e2e`. It forces Mesa software rendering, starts its own DBus session bus, runs an offscreen outer presentation, runs a normal inner presentation inside it, spawns real Weston clients, applies a split layout, and exports inner and outer debug dumps. This check is intended to cover real composition and capture without depending on a host GPU or host-visible presentation window.

See [[../workflows/test-nested]].

## Manual Host-Visible Harness

`tools/dev-nested-manifold.sh` is for manual development when a host-visible stable presentation is wanted. It is not the default automated agent verification path.

## Smoke Suites

Use `nix develop --command tools/audit-v1-capabilities.sh` for the concise acceptance audit. It runs Rust checks, headless compositor checks, host-backed checks when a host display is available, and DMA-BUF/explicit-sync capability checks.

Use `nix develop --command tools/smoke-v1-full.sh` for the larger full proof suite on a host display.

## Related Pages

- [[../workflows/test-nested]]
- [[runtime]]
- [[wayland-compositor]]
