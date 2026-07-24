# Manifold

Manifold is a new workspace project for a persistent, graphical developer environment.

The core idea is a local virtual workspace containing isolated sessions. A
session server owns durable state and hosted applications; an independent
presentation client opens the native viewport. The client is not a remote
framebuffer viewer. It imports individual pane surfaces and owns recursive
layout, composition, interaction, and presentation-local plugins.

## Thesis

Manifold should feel like a native editor while behaving more like a rich tmux session:

- managed apps and session state survive presentation window close/reopen
- editor, browser, terminal, agent, git, and app panes share one pane model
- browser and Wayland app surfaces are composited as first-class panes
- built-in apps can be WASM-backed plugins whose SDK components emit bounded canvas draw packets

## Initial Shape

- `manifold`: CLI entrypoint that starts the presentation client.
- `manifold-client`: WGPU presentation process that connects to the session server.
- `manifold-sessiond`: authoritative local session server.
- pane producers: browser pane client, terminal pane client, hosted Wayland app surfaces, and future native panes.
- plugins/WASM apps: built-in or user applications that use addressed actors, private shared-memory workers, completion-based I/O, WASI resources, optional sync policy lanes, canvas draw packets, native text services, and a future advanced-graphics escape hatch.

## Current Status

This repository contains a split session-server and presentation-client proof
of concept.

The current PoC can:

- expose a nested Wayland compositor socket
- launch and map hosted Wayland apps
- open an independent Winit/WGPU presentation client and render nested Wayland clients
- forward keyboard, pointer, button, and axis input
- close the presentation client while hosted Wayland apps keep running
- reconnect a new presentation client to the surviving session
- compose client-local plugin panes and managed panes through recursive layout leases
- report frame cadence, input-to-frame latency, client commits, frame callbacks, dmabuf state, and explicit-sync state
- run async core-WASM behavior harnesses through `manifold run --test-harness`

Graphical behavior tests start a headless server and a real external
`manifold-client`, then drive and capture that client through a headless Wayland
compositor. The backend is never used as an outer presentation container.

## Common Commands

Run the project through the Nix development shell:

```sh
nix develop --command cargo run
```

By default the CLI starts `manifold-client`. The client connects to the default
session server, starts `manifold-sessiond` when necessary, performs a protocol
handshake, and then opens its WGPU presentation window.

The session service is always usable without a presentation client. Service
managers and backend development invoke `manifold-sessiond`; exact command
options remain defined by the CLI parser.

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

For exact runtime, validation, and control commands, see the
[CLI reference pointers](docs/reference/cli.md).

Current acceptance uses the external presentation client rather than the old
daemon smoke matrix. Run the focused layout and session-switcher workloads
through the Nix shell, or use `nix run .#software-e2e-capture` for retained
client-composed frames and reconnect evidence. See
[Testing](docs/systems/testing.md) for the supported paths.

The session server uses Smithay and Wasmtime. The presentation client uses WGPU,
and shared CLI/DBus types live in `manifold-cli`. Local transport carries
per-window frames and bounded input/configuration lanes; viewport placement
never crosses that boundary.

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
- [Layout Actor Target](docs/systems/layout-actor.md)
- [Plugin UI Target](docs/systems/plugin-ui.md)
- [Plugin Graphics Target](docs/systems/plugin-graphics.md)
- [Future Advanced Plugin Graphics](docs/systems/plugin-advanced-graphics.md)
- [CLI Reference Pointers](docs/reference/cli.md)
- [Decision Records](docs/decisions/README.md)

Historical design notes and older prototypes live under `docs/archive/`.
