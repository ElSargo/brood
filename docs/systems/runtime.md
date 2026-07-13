---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-13
code-links:
  - crates/manifoldd/src/lib.rs
  - crates/manifoldd/src/compositor.rs
  - crates/manifoldd/src/state_services.rs
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
- load CLI-specified prototype WASM lanes
- start the DBus control service
- print `WAYLAND_DISPLAY` and DBus endpoint variables
- attach offscreen, attach Winit presentation, or remain headless
- load startup apps unless disabled
- spawn CLI command or browser if requested
- run the event loop

## Presentation Modes

Attached Winit presentation is the current product runtime default. The target architecture replaces direct attachment with an independent presentation client; see [[presentation-client]].

Headless runtime is for service-managed sessions and long-lived control endpoints. It is not the preferred behavior-test entry point.

Offscreen presentation is used by nested tests so composition can run without creating a host OS window.

## Session Runners

`run` may hand off to a [[../concepts/session-runner]] before serving the session. The Nix developer workflow deliberately bypasses configured session runners because the package already provides the runtime environment.

## Runtime State Ownership

The session thread remains the sole authority that commits compositor-visible
changes, but `Manifold` is an orchestrator rather than the owner of every
state transition:

- `SurfaceSessionService` owns the session catalogue, directory routing,
  surface assignments, interaction order, and session-local bus lifetimes.
  It validates allocation, activation, assignment, and removal before the
  compositor applies layout or focus effects. An empty catalogue has an
  explicit no-active-session state.
- `FocusService` owns focused and pointer-target identities plus pending
  presentation activation. Smithay seat focus remains an effect applied by
  the compositor after the service transition.
- `RegistryService` owns extension, window, grant, agent, notification, and
  status records. Agent notification input is typed as one request, and an
  unknown agent is rejected without a partial attention or notification
  mutation.
- `TestHarnessService` owns harness runtime/completion and nested target
  transitions. The compositor still decides when to service requests and stop
  the loop; process orchestration is not part of the state container.

`LayoutState` and `BrowserManager` are already independently owned service
boundaries. Geometry conversion policy and compositor/test process
orchestration are documented and evolved separately from these ownership
services.

The Nix-backed nested proof in
`tools/test-state-services-nested.sh` launches the real GTK session-switcher
extension, creates a directory-bound named session, publishes a substantive
agent notification, and captures the rendered switcher plus compositor state.
The screenshot and transcript expose the accepted behavior directly rather
than reducing the proof to a success flag.

## Related Pages

- [[presentation-client]]
- [[configuration]]
- [[testing]]
- [[plugin-runtime]]
- [[../workflows/run-manifold]]
- [[../workflows/enter-session]]
