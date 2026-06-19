# Manifold

Manifold is a new workspace project for a persistent, graphical developer environment.

The core idea is a local workspace containing isolated sessions. A local session process owns durable workspace/session state and can open a native presentation window. The presentation window is not a remote framebuffer viewer. It is a compositor endpoint that presents one session at a time, showing any number of that session's panes with presentation-surface-owned layout, animation, and interaction.

Unless a document explicitly calls out WSLg, Windows, or another backend, implementation prose defaults to the Linux-native Manifold path. When running on Windows, that same default should usually be read as the Windows/WSLg-native path instead.

## Thesis

Manifold should feel like a native editor while behaving more like a rich tmux session:

- managed apps and session state survive presentation window close/reopen
- editor, browser, terminal, agent, git, and app panes share one pane model
- browser and Wayland app surfaces are composited as first-class panes
- built-in apps can be sandboxed WASM modules that emit native scene primitives

## Initial Shape

- `manifoldd`: authoritative local session process and MVP presentation owner.
- `manifold-client`: deferred presentation endpoint for future isolation or multi-window attach experiments.
- pane producers: browser pane client, terminal pane client, hosted Wayland app surfaces, and future native panes.
- WASM apps: sandboxed built-in or user applications that emit scene patches.

## Current Status

This repository now contains a v1 compositor proof of concept in `manifoldd`.

The current PoC can:

- expose a nested Wayland compositor socket
- launch and map hosted Wayland apps
- open a Winit/GLES presentation window and render nested Wayland clients
- forward keyboard, pointer, button, and axis input
- close/detach the presentation window while hosted Wayland apps keep running
- reattach the presentation window and resume the nested window
- report frame cadence, input-to-frame latency, client commits, frame callbacks, dmabuf state, and explicit-sync state
- run async core-WASM behavior harnesses through `manifoldd run --test-harness`
- orchestrate nested Manifold-in-Manifold tests through `manifoldd test nested`

The intended default behavior-testing mode is nested Manifold-in-Manifold execution with outer-instance capture and full-path input driving. The built-in `manifoldd test nested` command is the canonical path toward that mode, even though headless outer capture still needs an offscreen presenter to be fully complete.

## Common Commands

Run the project:

```sh
cargo run
```

By default this starts `manifoldd run` and loads enabled modules from `./manifold.toml`. The checked-in config enables the local `dev` WASM key handler when its built `.wasm` artifact exists.

Run the tests:

```sh
cargo test
```

Useful variants:

```sh
cargo run -- check
cargo run -- run --headless
cargo run -- run --no-default-modules
cargo run -- run --config ./manifold.toml
cargo run -- ctl status-json
cargo run -- test nested
```

Run `./tools/audit-v1-capabilities.sh` for the current acceptance audit. It runs the full v1 smoke suite and separately checks positive dmabuf import plus strict explicit-sync validation. On the current WSLg backend, the full smoke suite passes and the positive dmabuf/sync checks skip with `rc=77` because WSLg exposes no importable dmabuf formats. On a DRM-capable backend, run `MANIFOLD_AUDIT_REQUIRE_NO_SKIPS=1 ./tools/audit-v1-capabilities.sh` to make those skips fail the audit.

Concrete choices for renderer stack, CEF integration, IPC, protocol encoding, WASM runtime, and crate layout are intentionally deferred. Remote presentation is a non-goal for the current prototype.

## Docs

- [Vision](docs/vision.md)
- [Shared Behavior Spec](docs/shared-behavior.md)
- [Architecture](docs/architecture.md)
- [Protocol](docs/protocol.md)
- [Rendering](docs/rendering.md)
- [Apps](docs/apps.md)
- [WASM Integration](docs/wasm-integration.md)
- [WASM Test Harness Prototype](docs/wasm-test-harness-prototype.md)
- [Browser](docs/browser.md)
- [Wayland](docs/wayland.md)
- [Development Environment](docs/dev-environment.md)
- [Decision Records](docs/decisions/README.md)
