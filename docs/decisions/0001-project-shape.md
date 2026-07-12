# 0001: Project Shape

## Status

Accepted as initial direction. Narrowed for the MVP by [0002: Local MVP Presentation](0002-local-mvp-presentation.md).

## Context

The project needs a persistent graphical developer workspace with editor panes, browser panes, terminal panes, agent workflows, rich text, git tools, and future nested Wayland support.

Earlier approaches considered an embedded GUI renderer inside Helix, an Electron root shell, and direct CEF embedding. Those approaches either centered the wrong process or made browser/native-surface integration the root of the application.

## Decision

Manifold will begin as a new standalone project centered on a session process and thin native presentation endpoints.

For the MVP prototype, this split is narrowed by [0002: Local MVP Presentation](0002-local-mvp-presentation.md): `manifold` owns the first local presentation window, and `manifold-client` is deferred.

- `manifold` owns authoritative session state.
- `manifold-client` owns local presentation execution when the presenter is split out.
- pane clients produce surfaces or scene data under session-process supervision.
- browser integration is modeled as a first-party pane client.
- WASM is the preferred portable plugin format and fault-isolation runtime.
  Authority isolation belongs to the enclosing session environment, as refined
  by [0006](0006-plugin-actors-and-workers.md).

## Consequences

- The renderer and protocol become first-class project concerns.
- Browser and Wayland integration can use the same pane/surface model.
- Sessions can eventually support attach, detach, reconnect, and multi-endpoint views.
- The first implementation must prove local responsiveness before expanding scope.

## Open Follow-Up

- Select renderer stack.
- Select protocol transport and encoding.
- Decide whether to fork or replace the current CEF binding approach.
- Define the first measurable latency benchmark.
- Define the minimal editor and browser pane prototype.
