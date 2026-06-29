# Architecture

Manifold is organized around a product-level workspace containing isolated sessions. The authoritative local session process can also own a presentation endpoint for the MVP.

The long-term architecture may split presentation into a separate endpoint process, but the first prototype keeps the session, nested Wayland compositor, and local presentation window in `manifoldd` to prove detach/reopen behavior with the fewest moving parts.

## Components

### `manifoldd`

The session process owns durable session truth and the wider workspace-level environment around it:

- sessions, session lifecycle, environment, cwd, capabilities, and producer membership
- entered session environments, including command context, GUI launch context, and optional isolation profile
- managed session isolation state: filesystem copy-on-write metadata, runtime
  IPC roots, network namespace metadata, PID/IPC namespace metadata, and DBus
  proxy configuration
- documents, buffers, selections, modes, commands, and pane records
- focus and input routing policy between panes and producers
- terminals, browser pane clients, Wayland app hosts, agents, and git models
- WASM app lifecycle, capability grants, and app state
- semantic scene commits and surface slots
- the MVP native presentation window and final composition path

### `manifold-client`

`manifold-client` is deferred for the MVP. It becomes relevant when Manifold needs a separate presentation endpoint for multi-window attach, crash isolation, or future multi-endpoint experiments.

A future presentation endpoint would own presentation execution:

- native window and input collection
- GL/EGL context and final composition
- glyph atlas, image cache, theme resources, and local animation clock
- pane visibility, placement, dragging, animation, smooth scroll interpolation, cursor blink, hover state, and hit testing
- imported GPU surfaces from browser, Wayland, terminal, or other pane producers

The presentation endpoint should not own authoritative editor, app, or pane state. It may save presentation preferences such as layout or visibility into the workspace. Pane producers can send presentation requests, but the presentation endpoint decides whether to honor them.

### Pane Producers

Pane producers are anything that can create or supply content for panes: supervised pane client processes, managed app surfaces, or WASM app views. A pane is the unit of presentation, while pane state lives behind an indirection layer in the session process, a WASM module, a managed app, or another producer.

First-party pane clients may include:

- browser pane client
- terminal pane client
- nested Wayland app host
- future native surface panes

Pane clients are not native child windows. They produce surfaces and metadata that the Manifold presentation compositor presents as panes.

### WASM Apps

WASM apps run in a sandboxed app host and emit scene patches using host-provided APIs. They may create any number of panes. They are intended for built-in tools such as git UI, settings, dashboards, inspectors, and agent panels.

WASM modules should interact with Manifold through a stable workspace/app API rather than compositor or renderer internals. The workspace core should model sessions, panes, pane producers, focus, commands, events, scene state, surface slots, and presentation requests before the WASM runtime becomes the primary extension point. Presentation layout and animation are presentation-surface concerns unless explicitly saved as workspace preferences.

See [WASM Integration](wasm-integration.md) for the planned host boundary, module roles, hot reload workflow, and milestones.

See [Resolved Layout](layout-mould.md) for the planned boundary between WASM-driven layout policy and native presentation execution.

See [Session Environments](session-environments.md) for the planned `enter`,
`show`, command-context, managed filesystem/runtime IPC, network namespace, and
DBus proxy model. See [Managed Session Isolation Plan](managed-session-isolation-plan.md)
for the staged implementation plan.

## MVP Data Flow

1. Hosted Wayland apps connect to the nested Wayland compositor owned by `manifoldd`.
2. `manifoldd` keeps hosted Wayland app connections and surface state alive across presentation window close/reopen.
3. The presentation endpoint imports managed app surfaces, updates scene caches, and composites the latest committed state at refresh rate.
4. Closing the presentation window detaches by default when there is state worth preserving.

## Entered Session Data Flow

1. `mctl enter`, `mctl show`, and related directory-scoped commands discover a
   project runner from the current directory, first through an ancestor
   `manifold.toml` and then through lower-priority project markers such as
   `flake.nix`, and resolve that project root to a project control target.
2. If the project session server is not already running, `mctl` starts it
   through the configured session runner before issuing the control command.
3. The session runner starts in a transient user scope, activates project tools
   such as `nix develop`, then resolves the launch record for its current
   cgroup to start the assigned session server or entered command.
4. `mctl enter` creates or reuses the session bound to the current directory when invoked from outside an entered session and builds an entered environment for a shell or command.
5. The entered environment carries the session DBus control endpoint and points GUI clients at the session's nested Wayland display.
6. The control service records which sessions are associated with each directory and which one is the default for future bare `enter` and `show` calls.
7. Optional isolation profiles create or join session-local filesystem, runtime
   IPC, network, PID/IPC, and DBus boundaries. The target `managed` profile
   captures regular writes by default, redirects runtime paths into the session,
   and keeps host bridges explicit.
8. Programs, agents, and GUI apps launched from the entered environment inherit those boundaries and appear as session participants.
9. `mctl show` presents the directory-bound or explicitly selected session without changing its process, filesystem, or network state, and can create an empty session when invoked outside one.
10. `mctl hide` resolves the same target shape and detaches the presentation surface without closing the session.
11. `mctl list` reports the directory-associated sessions by default and can report all live sessions globally.
12. Control commands and aliases such as `mctl` default to the entered session unless a command explicitly targets another session.

## Core Rule

The protocol carries semantic scene commits, input events, surface handles, and capability-scoped app messages. It must not depend on full-frame pixel streaming for local responsiveness.
