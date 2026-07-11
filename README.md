# Manifold

Manifold is a new workspace project for a persistent, graphical developer environment.

The core idea is a local virtual workspace containing isolated sessions. A local session process owns durable session state and can open a native presentation window. The presentation window is not a remote framebuffer viewer. It is a compositor endpoint that presents one session at a time, showing any number of that session's panes with presentation-surface-owned layout, animation, and interaction.

## Thesis

Manifold should feel like a native editor while behaving more like a rich tmux session:

- managed apps and session state survive presentation window close/reopen
- editor, browser, terminal, agent, git, and app panes share one pane model
- browser and Wayland app surfaces are composited as first-class panes
- built-in apps can be sandboxed plugins, often WASM-backed, that describe native component panes or scene primitives

## Initial Shape

- `manifold`: authoritative local session process and MVP presentation owner.
- `manifold-client`: deferred presentation endpoint for future isolation or multi-window attach experiments.
- pane producers: browser pane client, terminal pane client, hosted Wayland app surfaces, and future native panes.
- plugins/WASM apps: sandboxed built-in or user applications that use addressed actors, private shared-memory workers, completion-based I/O, WASI resources, optional sync policy lanes, and host-rendered UI or scene patches.

## Current Status

This repository now contains a v1 compositor proof of concept in `manifold`.

The current PoC can:

- expose a nested Wayland compositor socket
- launch and map hosted Wayland apps
- open a Winit/GLES presentation window and render nested Wayland clients
- forward keyboard, pointer, button, and axis input
- close/detach the presentation window while hosted Wayland apps keep running
- reattach the presentation window and resume the nested window
- report frame cadence, input-to-frame latency, client commits, frame callbacks, dmabuf state, and explicit-sync state
- run async core-WASM behavior harnesses through `manifold run --test-harness`
- orchestrate nested Manifold-in-Manifold tests through `manifold test nested`

The intended default behavior-testing mode is nested Manifold-in-Manifold execution with outer-instance capture and full-path input driving. The built-in `manifold test` command defaults to that mode. When it is launched from inside an existing Manifold session, it reuses that session as the outer container and starts only the subject instance.

## Common Commands

Run the project through the Nix development shell:

```sh
nix develop --command cargo run
```

By default this starts `manifold run`, attaches a presentation surface, and
loads enabled modules and startup apps from the global Manifold config. Manifold
first checks the XDG config location and then falls back to the checked-in
`manifold.toml` found from the development binary's repo checkout. The checked-in
config enables the target-shape default-workspace WASM plugin when its built
artifact exists and starts the pi agent workspace pane.

Run Rust unit tests through Nix:

```sh
nix develop --command cargo test
```

Run Manifold behavior tests through Nix:

```sh
nix develop --command cargo run -- test
```

Behavior tests own their presentation policy. Outside Manifold, the default test topology starts an offscreen-attached outer supervisor and a normal subject compositor inside it, so graphical behavior can be exercised without opening a host OS window. Inside a managed Manifold session, `manifold_session` is present, so the test command skips the extra outer supervisor and starts only the subject compositor.

For automated graphical behavior checks, keep using `test`, for example:

```sh
nix develop --command cargo run -- test nested --inner-command 'weston-simple-egl -f'
```

The outer supervisor is offscreen-attached unless `--outer-headful` is explicitly
passed.
The older helper script `tools/dev-nested-manifold.sh` is for host-visible manual
development and opens a presentation window on the current desktop.

Run the session service without attaching it to a desktop presentation:

```sh
nix develop --command cargo run -- run --headless
```

`run --headless` is for service managers and long-lived runtime processes, such as a future systemd unit. Use `test` rather than `run --headless` for behavior validation.

The repo includes `flake.nix`, and checked-in development workflows should prefer it for builds and tests. Its default package is the Manifold app; heavier development environments are exposed through explicit `full` outputs.

Start the one-line development workflow from Nix:

```sh
nix run github:ElSargo/manifold#dev-workflow
```

This clones a writable checkout under `~/.cache`, installs the packaged WASM
example artifacts used by the normal config, starts a normal Manifold session
from the Nix-built binary, and opens a terminal in that checkout. See
[Develop With Nix](docs/workflows/develop-with-nix.md).

For the larger toolbox with Docker, Go, Firefox, and smoke tools:

```sh
nix run github:ElSargo/manifold#dev-workflow-full
```

Useful variants:

```sh
nix develop --command cargo run -- check
nix develop --command cargo run -- run --no-default-modules
nix develop --command cargo run -- run --config ./manifold.toml
nix develop --command cargo run -- ctl status-json
nix develop --command cargo run -- test
nix develop --command cargo run -- test nested
nix develop --command cargo run -- test nested --force-outer
```

Run `./tools/audit-v1-capabilities.sh` for the current acceptance audit. It runs the supported v1 smoke suite and separately checks positive dmabuf import plus strict explicit-sync validation. On a backend without importable dmabuf formats, those positive checks skip with `rc=77`. On a DRM-capable backend, run `MANIFOLD_AUDIT_REQUIRE_NO_SKIPS=1 ./tools/audit-v1-capabilities.sh` to make those skips fail the audit.

The current prototype uses Smithay with Winit/EGL for presentation, Wasmtime for
WASM, DBus plus in-process channels for control, and a two-crate Rust workspace.
CEF integration, durable plugin protocol encoding, and longer-term crate
boundaries remain open. Remote presentation is a non-goal for the current
prototype.

## Docs

The docs are organized as a wiki-style knowledge base.

- [Knowledge Base Index](docs/index.md)
- [Overview](docs/overview.md)
- [Architecture](docs/architecture.md)
- [Runtime](docs/systems/runtime.md)
- [Testing](docs/systems/testing.md)
- [Configuration](docs/systems/configuration.md)
- [Extension Host](docs/systems/extension-host.md)
- [Plugin Runtime Target](docs/systems/plugin-runtime.md)
- [Input And Shortcuts Target](docs/systems/input-shortcuts.md)
- [Layout Policy Target](docs/systems/layout-policy.md)
- [Plugin UI Target](docs/systems/plugin-ui.md)
- [CLI Reference Pointers](docs/reference/cli.md)
- [Decision Records](docs/decisions/README.md)

Historical design notes and older prototypes live under `docs/archive/`.
