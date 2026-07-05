# Nested Development Harness

This page describes the nested-development and nested-testing target shape by default.

Current implementation details are included only where they affect usability.

Manifold should be able to run and test a development instance inside a stable instance. This gives the project a realistic way to exercise input routing, frame pacing, screenshot capture, hot-reload behavior, and latency metrics with Manifold acting as both the outer workspace and the inner system under test.

## Shape

The nested test topology uses two normal `manifold` processes:

```text
host compositor
  -> stable manifoldd presentation target
    -> dev manifoldd presentation window as a stable hosted app
      -> dev nested client, test harness, or WASM-controlled app
```

The stable instance owns the outer presentation target. In automated nested tests that target is an offscreen framebuffer rather than a host OS window. The dev instance is launched through the stable DBus control endpoint, so its presentation endpoint connects to the stable nested Wayland display and appears as a pane inside the stable workspace.

Both instances expose their own DBus control endpoints:

- stable DBus control endpoint: drives the outer workspace, captures composed presentation output, injects input into the dev presentation window
- dev DBus control endpoint: drives the inner workspace, launches test clients, samples inner frame/input/commit status

## Built-In CLI

The first built-in orchestration path is:

```sh
manifold test
```

This launches:

- an outer supervisor instance
- an inner system-under-test instance hosted inside the outer instance

`manifold test` defaults to the nested topology. `manifold test nested` is the explicit spelling for the same workflow.

This topology is the intended default test mode for behavior and graphics validation. The outer instance should be the authoritative end-to-end supervisor:

- drive the full outer-to-inner input path
- capture host-visible or offscreen-composed output
- record outer-visible latency and frame metrics
- proxy diagnostic inspection into the inner instance when a deviation appears

By default the outer supervisor uses an attached offscreen presentation target, so the nested topology can start without a host-visible presentation window while still running composition, frame callbacks, capture, layout, focus, and nested Wayland behavior through the presentation path. The inner instance still attaches a normal presentation window into the outer nested compositor.

When Manifold launches a hosted app, it sets `manifold_session` in that app's environment. If `manifold test` sees that marker, it treats the current Manifold session as the outer container and starts only the subject compositor. Use `manifold test nested --force-outer` when a test must create another explicit outer supervisor even though it is already running inside Manifold.

For exact `manifold test nested` arguments, see `crates/manifoldd/src/main.rs`.

When the topology is running, the outer instance can proxy control requests to the inner instance:

```sh
manifold ctl --dbus-name <outer-destination> nested-inner status-json
```

For automated agent verification, this built-in CLI is the default path. Use it
instead of the helper script when the test should not create an OS window in the
current desktop session. A typical real-client behavior check starts the nested
test topology with an inner client command and a temporary artifact directory,
then inspects the printed DBus control endpoints, status, logs, and debug dumps:

```sh
nix develop --command cargo run -- test nested --out-dir /tmp/manifold-test --inner-command 'weston-simple-egl -f'
```

Use `--keep-running` only when the verification script also cleans up the
printed inner and outer DBus control endpoints. The outer supervisor remains
offscreen-attached unless the headful outer presentation mode is explicitly
requested. The artifact directory includes an outer debug dump with the composed
framebuffer capture.

## Inner Command Wrappers

`--inner-command` is for the native client or helper executable that should run
inside the subject compositor's nested Wayland display. The built-in test
orchestrator splits this command into argv words and sends it through the normal
DBus `spawn` path; it is not a shell script parser.

For anything beyond a single executable with simple flags, put the behavior in a
temporary executable wrapper and pass the wrapper path as the inner command. A
wrapper is the right place to set client-specific environment variables, redirect
logs, enable line buffering, choose a private profile directory, or preserve
shell quoting that would otherwise be lost before the command reaches `spawn`.

Typical wrapper shape:

```sh
#!/usr/bin/env sh
exec stdbuf -oL -eL weston-eventdemo \
  --log-motion --log-button --log-key --log-axis \
  >"$ARTIFACT_DIR/weston-eventdemo.log" 2>&1
```

The wrapper runs as the hosted client process. It inherits the subject
compositor's nested Wayland environment from Manifold, so it does not need to
discover or set `WAYLAND_DISPLAY` itself unless the test is intentionally
overriding that environment.

Wrappers also keep failure attribution clearer. If the wrapper exits nonzero,
the nested test reports a failed hosted child. If it exits zero without mapping a
window, the run can still succeed for protocol-only clients such as
`wayland-info`. If a GUI client is expected to stay alive and render, the test
should assert mapped windows, frame progress, and any client log markers it cares
about.

## WASM Harness Composition

The WASM test harness is the controller and assertion layer; an inner-command
wrapper is just one possible workload launched inside the subject compositor.
They compose in two useful ways:

- Start the wrapper with `--inner-command`, then let the WASM harness inspect
  status, inject input, send control commands, wait for signals, and decide
  pass/fail.
- Put wrappers on `PATH` or pass their paths through harness-specific config, so
  the WASM harness can spawn native clients itself during the scenario.

In the outer-supervisor topology, the harness is loaded into the outer process.
It can drive the full stable-to-subject path and use the `nested-inner` control
proxy when it needs to inspect or command the subject instance. In the
current-session topology, there is no extra outer process, so the harness runs in
the subject process and controls that instance directly.

This separation is intentional. Native wrappers cover messy process-launch
details for real clients; the WASM harness owns scenario policy, synchronization,
status checks, and pass/fail reporting. A harness should not depend on fragile
inline shell quoting to create the client process it wants to test.

## Success Criteria

For `manifold test nested`, success starts with the command exiting zero, but a
useful behavior test should also validate the artifacts that match its topology:

- outer-supervisor tests should record an outer debug dump, a nonempty composed
  capture, attached presentation status, frame progress, and the expected
  presentation target
- offscreen tests should show `presentation_target=offscreen` for the outer
  supervisor
- visible outer tests should show `presentation_target=winit` for the outer
  supervisor
- GUI-client tests should prove that the expected windows mapped and frames
  advanced
- protocol-only client tests should prove the client exited successfully and
  produced the expected protocol/status output
- input tests should compare outer and inner input counters and, when possible,
  client-visible logs
- WASM harness tests should contain a passing `TEST_HARNESS_RESULT` and no
  failed or paused harness result

Logs are diagnostic evidence, not the only success oracle. A visible Winit run
can recover from a transient host EGL surface loss while still producing a valid
capture and clean status sample. Treat repeated presentation runtime failures,
missing captures, missing frame progress, child-process failures, protocol
errors, or failed harness results as test failures.

## Headless Runtime

`manifold run --headless` remains part of the runtime surface. It is for service-managed sessions that should expose a nested compositor and DBus control endpoint without being tied to a specific host presentation session. It is not the default behavior-test entry point; test workflows should go through `manifold test` so the nesting policy can choose the right topology for the current environment.

## Harness

Run:

```sh
tools/dev-nested-manifold.sh --keep-running
```

This helper is the repo-local host-visible manual development harness. It starts the stable
outer Manifold with a presentation window on the current desktop, then hosts the
dev Manifold inside that window. Do not use it for automated no-host-window
behavior tests; use `manifold test` for that.

The script:

1. Builds `manifold`.
2. Starts a stable instance on the real host display.
3. Spawns a dev instance inside the stable instance.
4. Waits for stable to map and render the dev presentation window.
5. Spawns a real client inside the dev instance.
6. Samples `status-json` from both instances.

For exact helper-script arguments, see `tools/dev-nested-manifold.sh`.

The output directory contains:

- `stable.log`
- `dev.log`
- `stable-status.jsonl`
- `dev-status.jsonl`

## Input Testing

There are two useful input paths:

- Host or stable input into the dev presentation window, validating the full outer-to-inner routing path.
- Direct dev DBus control endpoint injection, validating the inner compositor and client path without depending on outer hit testing.

The long-term benchmark should drive both paths. The full-path benchmark proves Manifold can operate on itself. The direct inner benchmark keeps regressions easier to isolate.

## Hot Reload Direction

The nested setup is a good place to test hot reload because the stable instance can keep the dev presentation and control surface alive while the dev instance reloads user code or test harnesses.

First useful reload targets:

- WASM key handler reload
- controller/module reload
- scene producer reload
- test-harness command reload

Reload validation should record:

- whether pane identity survived
- whether DBus control endpoints stayed responsive
- whether frame counters continued after reload
- whether input-to-frame and commit-to-frame telemetry stayed within budget
- whether stale module state or failed reloads appeared in status/logs

## Latency And Frame Analysis

The current status surfaces already expose useful counters:

- `frames`
- `last_frame_interval_us`
- `max_frame_interval_us`
- `last_input_to_frame_us`
- `max_input_to_frame_us`
- `last_commit_to_frame_us`
- `max_commit_to_frame_us`
- `last_commit_to_submit_us`
- `max_commit_to_submit_us`

The nested harness records both stable and dev `status-json` samples. Analysis should compare:

- outer stable frame cadence
- inner dev frame cadence
- input events injected into stable versus visible frames in stable
- input events injected into dev versus visible frames in dev
- dev client commits versus dev presentation frames
- dev presentation commits as observed by stable

That makes it possible to tell whether a regression lives in the stable outer compositor, the dev inner compositor, the nested client, or the host presentation path.

## Near-Term Gaps

- The stable instance does not yet know how to crop captures to the dev pane.
- The harness samples status but does not yet compute p50/p95/p99 latency summaries.
- Hot reload is not wired into the control API yet.
- Dev process supervision is still shell-based because the current `spawn` control command only launches argv inside the stable session.
- Full-path input is available through stable input injection, but target selection still depends on current focus/hit testing behavior.
