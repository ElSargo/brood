---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-01
code-links:
  - crates/manifoldd/src/lib.rs
  - crates/manifoldd/src/compositor.rs
  - crates/manifoldd/src/config.rs
  - crates/manifoldd/src/session_runner.rs
---

# Runtime

The default `manifold` invocation starts the `run` command. The runtime creates a nested Wayland compositor, DBus control endpoint, initial surface session, startup modules, startup apps, and an attached presentation unless `--headless` or hidden test flags change that policy.

Exact flags are defined in `crates/manifoldd/src/lib.rs`; do not duplicate the full parser here.

## Startup Order

Current high-level startup order:

- resolve the config path
- create the `Manifold` state and Wayland display
- optionally clear the initial surface session
- load config modules unless disabled
- load CLI-specified WASM lanes
- start the DBus control service
- print `WAYLAND_DISPLAY` and DBus endpoint variables
- attach offscreen, attach Winit presentation, or remain headless
- load startup apps unless disabled
- spawn CLI command or browser if requested
- run the event loop

## Presentation Modes

Attached Winit presentation is the product runtime default.

Headless runtime is for service-managed sessions and long-lived control endpoints. It is not the preferred behavior-test entry point.

Offscreen presentation is used by nested tests so composition can run without creating a host OS window.

## Session Runners

`run` may hand off to a [[../concepts/session-runner]] before serving the session. The Nix developer workflow deliberately bypasses configured session runners because the package already provides the runtime environment.

## Related Pages

- [[configuration]]
- [[testing]]
- [[../workflows/run-manifold]]
- [[../workflows/enter-session]]
