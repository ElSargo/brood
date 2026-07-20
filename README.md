# Manifold

Manifold is a new workspace project for a persistent, graphical developer environment.

The core idea is a local virtual workspace containing isolated sessions. A local session process owns durable session state and can open a native presentation window. The presentation window is not a remote framebuffer viewer. It is a compositor endpoint that presents one session at a time, showing any number of that session's panes with presentation-surface-owned layout, animation, and interaction.

## Thesis

Manifold should feel like a native editor while behaving more like a rich tmux session:

- managed apps and session state survive presentation window close/reopen
- editor, browser, terminal, agent, git, and app panes share one pane model
- browser and Wayland app surfaces are composited as first-class panes
- built-in apps can be sandboxed plugins, often WASM-backed, that describe native component panes or submit custom render packets

## Initial Shape

- `manifold`: CLI entrypoint that starts the presentation client.
- `manifold-client`: WGPU presentation process that connects to the session server.
- `manifold-sessiond`: authoritative local session server.
- pane producers: browser pane client, terminal pane client, hosted Wayland app surfaces, and future native panes.
- plugins/WASM apps: sandboxed built-in or user applications that use addressed actors, private shared-memory workers, completion-based I/O, WASI resources, optional sync policy lanes, host-rendered UI, or low-level pane graphics.

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

By default the CLI starts `manifold-client`. The client connects to the default
session server, starts `manifold-sessiond` when necessary, performs a protocol
handshake, and then opens its WGPU presentation window.

Run the session service without attaching it to a desktop presentation:

```sh
nix develop --command cargo run -- run --headless
```

`run --headless` remains available for service managers and explicit server development.

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
```

Run `./tools/audit-v1-capabilities.sh` for the current acceptance audit. It runs the supported v1 smoke suite and separately checks positive dmabuf import plus strict explicit-sync validation. On a backend without importable dmabuf formats, those positive checks skip with `rc=77`. On a DRM-capable backend, run `MANIFOLD_AUDIT_REQUIRE_NO_SKIPS=1 ./tools/audit-v1-capabilities.sh` to make those skips fail the audit.

The session server uses Smithay and Wasmtime. The presentation client uses WGPU,
and shared CLI/DBus types live in `manifold-cli`. Durable presentation transport
and rendering beyond the initial handshake and window remain open.

## Docs

The docs are organized as a wiki-style knowledge base.

- [Knowledge Base Index](docs/index.md)
- [Overview](docs/overview.md)
- [Architecture](docs/architecture.md)
- [Runtime](docs/systems/runtime.md)
- [Testing](docs/systems/testing.md)
- [Configuration](docs/systems/configuration.md)
- [Extension Host](docs/systems/extension-host.md)
- [Presentation Client Target](docs/systems/presentation-client.md)
- [Plugin Runtime Target](docs/systems/plugin-runtime.md)
- [Input And Shortcuts Target](docs/systems/input-shortcuts.md)
- [Layout Policy Target](docs/systems/layout-policy.md)
- [Plugin UI Target](docs/systems/plugin-ui.md)
- [Plugin Graphics Target](docs/systems/plugin-graphics.md)
- [CLI Reference Pointers](docs/reference/cli.md)
- [Decision Records](docs/decisions/README.md)

Historical design notes and older prototypes live under `docs/archive/`.
