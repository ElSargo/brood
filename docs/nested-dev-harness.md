# Nested Development Harness

Manifold should be able to run and test a development instance inside a stable instance. This gives the project a realistic way to exercise input routing, frame pacing, screenshot capture, hot-reload behavior, and latency metrics with Manifold acting as both the outer workspace and the inner system under test.

## Shape

The first version uses two normal `manifoldd` processes:

```text
host compositor / WSLg
  -> stable manifoldd presentation window
    -> dev manifoldd presentation window as a stable hosted app
      -> dev nested client, test harness, or WASM-controlled app
```

The stable instance owns the host-facing presentation window. The dev instance is launched through the stable control socket, so its presentation endpoint connects to the stable nested Wayland display and appears as a pane inside the stable workspace.

Both instances expose their own control sockets:

- stable control socket: drives the outer workspace, captures host-visible presentation, injects input into the dev presentation window
- dev control socket: drives the inner workspace, launches test clients, samples inner frame/input/commit status

## Built-In CLI

The first built-in orchestration path is:

```sh
manifoldd test nested
```

This launches:

- an outer supervisor instance
- an inner system-under-test instance hosted inside the outer instance

This nested topology is the intended default test mode for behavior and graphics validation. The outer instance should be the authoritative end-to-end supervisor:

- drive the full outer-to-inner input path
- capture host-visible or offscreen-composed output
- record outer-visible latency and frame metrics
- proxy diagnostic inspection into the inner instance when a deviation appears

By default the outer supervisor runs headless, so the nested topology can start without a host-visible presentation window. The inner instance still attaches a presentation window into the outer nested compositor, which preserves the Manifold-in-Manifold path even though the outer host presentation is detached.

Useful options:

```sh
manifoldd test nested --keep-running
manifoldd test nested --harness ./outer_test.wasm
manifoldd test nested --outer-headful
manifoldd test nested --inner-command kitty
```

When the topology is running, the outer instance can proxy control requests to the inner instance:

```sh
manifoldd ctl --control-socket /path/to/outer.sock nested-inner status-json
```

Current limitation: headless outer mode preserves behavior and control-path testing, but it does not yet provide a composed outer framebuffer capture. That still requires either an attached outer presentation window or a future offscreen presenter.

## Future Work

The most important remaining gap for the intended default nested test mode is full outer capture while staying headless by default.

Planned work:

- add an offscreen outer presenter so `manifoldd test nested` can capture the composed outer framebuffer without opening a host-visible window
- retain the same nested topology, control flow, and latency instrumentation when switching between outer-headful and outer-headless execution
- make full capture part of the default nested artifact bundle rather than a special-case debugging path
- keep inner-instance dumps and diagnostics as supporting evidence, while outer composed capture remains the authoritative end-to-end visual record

## Harness

Run:

```sh
tools/dev-nested-manifold.sh --keep-running --capture
```

The script:

1. Builds `manifoldd`.
2. Starts a stable instance on the real host display.
3. Spawns a dev instance inside the stable instance.
4. Waits for stable to map and render the dev presentation window.
5. Spawns a real client inside the dev instance.
6. Samples `status-json` from both instances.
7. Optionally captures the stable host window through WSLg `PrintWindow`.

Useful options:

```sh
tools/dev-nested-manifold.sh --dev-client kitty
tools/dev-nested-manifold.sh --dev-client "weston-simple-egl"
tools/dev-nested-manifold.sh --out-dir /tmp/manifold-nested --capture
```

The output directory contains:

- `stable.log`
- `dev.log`
- `stable-status.jsonl`
- `dev-status.jsonl`
- optional capture PNG

## Input Testing

There are two useful input paths:

- Host or stable input into the dev presentation window, validating the full outer-to-inner routing path.
- Direct dev control-socket injection, validating the inner compositor and client path without depending on outer hit testing.

The long-term benchmark should drive both paths. The full-path benchmark proves Manifold can operate on itself. The direct inner benchmark keeps regressions easier to isolate.

## Output Capture

On WSLg, capture the exported stable presentation window with `tools/wslg-capture-manifold-window.ps1`. This captures what the user sees: stable presentation, dev presentation inside it, and any dev-hosted client.

Later harnesses should also support compositor-local frame capture from the presentation renderer. That would make pixel and latency analysis less dependent on WSLg window export behavior, while the WSLg capture remains useful as an end-to-end proof.

## Hot Reload Direction

The nested setup is a good place to test hot reload because the stable instance can keep the dev presentation and control surface alive while the dev instance reloads user code or test harnesses.

First useful reload targets:

- WASM key handler reload
- controller/module reload
- scene producer reload
- test-harness command reload

Reload validation should record:

- whether pane identity survived
- whether control sockets stayed responsive
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
