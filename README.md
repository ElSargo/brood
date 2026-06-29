# Manifold

Manifold is a new workspace project for a persistent, graphical developer environment.

The core idea is a local virtual workspace containing isolated sessions. A local session process owns durable session state and can open a native presentation window. The presentation window is not a remote framebuffer viewer. It is a compositor endpoint that presents one session at a time, showing any number of that session's panes with presentation-surface-owned layout, animation, and interaction.

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

The intended default behavior-testing mode is nested Manifold-in-Manifold execution with outer-instance capture and full-path input driving. The built-in `manifoldd test` command defaults to that mode. When it is launched from inside an existing Manifold session, it reuses that session as the outer container and starts only the subject instance.

## Common Commands

Run the project:

```sh
cargo run
```

By default this starts `manifoldd run`, attaches a presentation surface, and loads enabled modules and startup apps from `./manifold.toml`. The checked-in config enables the local `dev` WASM key handler when its built `.wasm` artifact exists and starts the Codex agent workspace pane. This is the end-user path.

Run Rust unit tests:

```sh
cargo test
```

Run Manifold behavior tests:

```sh
cargo run -- test
```

Behavior tests own their presentation policy. Outside Manifold, the default test topology starts an offscreen-attached outer supervisor and a normal subject compositor inside it, so graphical behavior can be exercised without opening a host OS window. Inside a managed Manifold session, `manifold_session` is present, so the test command skips the extra outer supervisor and starts only the subject compositor.

For automated graphical behavior checks, keep using `test`, for example:

```sh
cargo run -- test nested --inner-command 'weston-simple-egl -f'
```

The outer supervisor is offscreen-attached unless `uu--outer-headful` is explicitly
passed.
The older helper script `tools/dev-nested-manifold.sh` is for host-visible manual
development and opens a presentation window on the current desktop.

Run the session service without attaching it to a desktop presentation:

```sh
cargo run -- run --headless
```

`run --headless` is for service managers and long-lived runtime processes, such as a future systemd unit. Use `test` rather than `run --headless` for behavior validation.

The repo also includes `flake.nix` for developers who want a Nix shell. It is optional; the ordinary Cargo commands above are the default project entry points.

Start the one-line development workflow from Nix:

```sh
nix run github:ElSargo/manifold#dev-workflow
```

This clones a writable checkout under `~/.cache`, builds the WASM example
artifacts used by the normal config, starts a normal Manifold session from the
crate2nix-built binary, and opens a terminal in that checkout. See
[Development Environment](docs/dev-environment.md).

Useful variants:

```sh
cargo run -- check
cargo run -- run --no-default-modules
cargo run -- run --config ./manifold.toml
cargo run -- ctl status-json
cargo run -- test
cargo run -- test nested
cargo run -- test nested --force-outer
```

Run `./tools/audit-v1-capabilities.sh` for the current acceptance audit. It runs the supported v1 smoke suite and separately checks positive dmabuf import plus strict explicit-sync validation. On a backend without importable dmabuf formats, those positive checks skip with `rc=77`. On a DRM-capable backend, run `MANIFOLD_AUDIT_REQUIRE_NO_SKIPS=1 ./tools/audit-v1-capabilities.sh` to make those skips fail the audit.

Concrete choices for renderer stack, CEF integration, IPC, protocol encoding, WASM runtime, and crate layout are intentionally deferred. Remote presentation is a non-goal for the current prototype.

## Docs

- [Vision](docs/vision.md)
- [Shared Behavior Spec](docs/shared-behavior.md)
- [Architecture](docs/architecture.md)
- [Protocol](docs/protocol.md)
- [Rendering](docs/rendering.md)
- [Apps](docs/apps.md)
- [WASM Integration](docs/wasm-integration.md)
- [WASM Realms](docs/wasm-realms.md)
- [WASM Test Harness Prototype](docs/wasm-test-harness-prototype.md)
- [Browser](docs/browser.md)
- [Wayland](docs/wayland.md)
- [Development Environment](docs/dev-environment.md)
- [Decision Records](docs/decisions/README.md)
