---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-01
code-links:
  - crates/manifoldd/src/compositor.rs
  - tools/audit-v1-capabilities.sh
  - tools/smoke-v1-full.sh
---

# Testing

The supported behavior-test path is the built-in `manifold test` CLI. It owns nested topology policy and avoids creating a host OS window by default.

Use product runtime commands such as `manifold run` for runtime/manual use, not automated behavior validation.

## Built-In Nested Tests

`manifold test` defaults to `manifold test nested`.

Outside Manifold, the default topology starts an outer supervisor with offscreen presentation, then starts the subject compositor inside it. Inside an existing Manifold session, the test command detects `manifold_session` and starts only the subject compositor unless `--force-outer` is used.

The outer supervisor is offscreen-attached unless `--outer-headful` is passed.

See [[../workflows/test-nested]].

## Manual Host-Visible Harness

`tools/dev-nested-manifold.sh` is for manual development when a host-visible stable presentation is wanted. It is not the default automated agent verification path.

## Smoke Suites

Use `tools/audit-v1-capabilities.sh` for the concise acceptance audit. It runs Rust checks, headless compositor checks, host-backed checks when a host display is available, and DMA-BUF/explicit-sync capability checks.

Use `tools/smoke-v1-full.sh` for the larger full proof suite on a host display.

## Related Pages

- [[../workflows/test-nested]]
- [[runtime]]
- [[wayland-compositor]]
