# Architecture

Manifold is organized around an authoritative local session process that can also own a presentation endpoint for the MVP.

The long-term architecture may split presentation into a separate endpoint process, but the first prototype keeps the session, nested Wayland compositor, and local presentation window in `manifoldd` to prove detach/reopen behavior with the fewest moving parts.

## Components

### `manifoldd`

The session process owns workspace truth:

- documents, buffers, selections, modes, commands, and pane tree
- focus, input routing, layouts, and session lifecycle
- terminals, browser pane clients, Wayland app hosts, agents, and git models
- WASM app lifecycle, capability grants, and app state
- semantic scene commits and surface placement
- the MVP native presentation window and final composition path

### `manifold-client`

`manifold-client` is deferred for the MVP. It becomes relevant when Manifold needs a separate presentation endpoint for multi-window attach, crash isolation, or future multi-endpoint experiments.

A future presentation endpoint would own presentation execution:

- native window and input collection
- GL/EGL context and final composition
- glyph atlas, image cache, theme resources, and local animation clock
- smooth scroll interpolation, cursor blink, hover state, and hit testing
- imported GPU surfaces from browser, Wayland, terminal, or other pane producers

The presentation endpoint should not own authoritative editor state.

### Pane Producers

Pane producers are anything that supplies content for a pane: supervised pane client processes, managed app surfaces, or WASM app views.

First-party pane clients may include:

- browser pane client
- terminal pane client
- nested Wayland app host
- future native surface panes

Pane clients are not native child windows. They produce surfaces and metadata that the Manifold presentation compositor presents.

### WASM Apps

WASM apps run in a sandboxed app host and emit scene patches using host-provided APIs. They are intended for built-in tools such as git UI, settings, dashboards, inspectors, and agent panels.

## MVP Data Flow

1. Hosted Wayland apps connect to the nested Wayland compositor owned by `manifoldd`.
2. `manifoldd` keeps hosted Wayland app connections and surface state alive across presentation window close/reopen.
3. The presentation endpoint imports managed app surfaces, updates scene caches, and composites the latest committed state at refresh rate.
4. Closing the presentation window detaches by default when there is state worth preserving.

## Core Rule

The protocol carries semantic scene commits, input events, surface handles, and capability-scoped app messages. It must not depend on full-frame pixel streaming for local responsiveness.
