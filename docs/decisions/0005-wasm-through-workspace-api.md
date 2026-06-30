# 0005: WASM Modules Use The Workspace API

## Context

Manifold wants WASM integration for fast experimentation, workspace behavior control, stateful WASM pane apps, and capability-scoped interaction between apps, controllers, extensions, agents, managed apps, and the host.

The current Linux prototype keeps session state, Smithay compositor state, and the MVP presentation endpoint together in `manifold`. Binding WASM directly to Smithay, EGL, DMA-BUF, or renderer internals would make the Linux path fast at first but would leak backend details into the app API and make later backend work harder.

## Decision

WASM modules will interact with Manifold through a stable workspace/app API rather than compositor or renderer internals.

The first implementation milestone should create a native session core, with any needed workspace-level model around it, for sessions, panes as presentation units, pane producers, focus, commands, events, scene state, and surface slots. Presentation layout, dragging, and animation belong to the presentation surface, with saved preferences stored in the workspace when needed. The WASM app host should then become an adapter over that model. Linux-native `manifold` can host Wasmtime in-process for fast iteration, while later backend work can satisfy the same behavior through a protocol-facing session process or backend-specific adapter.

## Consequences

- The WASM API can remain portable across later backend work.
- The runtime can be hot-reloaded without rebuilding compositor internals.
- The first WASM work must start with workspace and scene model boundaries, not just embedding Wasmtime.
- Some early Linux experiments may take longer because direct Smithay access is intentionally avoided.

## Follow-Up

- Build the native workspace core before loading arbitrary WASM modules.
- Draft versioned WIT APIs after the native Rust API proves the first useful shape.
- Add diagnostics for module lifecycle, capabilities, hot reload, event backlog, and scene patch counters.
