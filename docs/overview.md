---
status: canonical
type: overview
authority: product
last-reviewed: 2026-07-13
---

# Overview

Manifold is a local persistent graphical workspace for development. It explores whether an extra compositor/session layer can feel native while preserving app and agent state across presentation attach, detach, and switching.

The product model is:

- A [[concepts/workspace]] contains one or more [[concepts/session]]s.
- A session groups panes, programs, agents, environment, cwd, and capabilities.
- A [[concepts/presentation-surface]] is an independent client viewport that shows one session at a time.
- A [[concepts/pane]] is the unit of presentation.
- A [[concepts/managed-app]] is an external native app tracked by Manifold.
- A [[concepts/plugin]] can extend behavior through addressed actors, private actor-scoped workers, completion-based I/O, WASI resources, policy providers, and publications.
- A [[concepts/wasm-controller]] is one plugin role for changing workspace behavior through host APIs.

The current Rust workspace contains `manifoldd`, which builds the user-facing
`manifold` binary, and `manifold-wasm`, which implements the hosted WASM
runtimes. The daemon binary currently owns the session process, nested Wayland
compositor, DBus control endpoint, and MVP presentation endpoint. The target
architecture extracts a portable presentation client with client-side WASM and
WGPU rendering; see [[systems/presentation-client]].

See [[architecture]] for the implementation map.

## Current Focus

The repository is currently proving:

- nested Wayland app hosting
- native presentation attach/detach
- keyboard and pointer forwarding
- session switching
- startup modules and startup apps from config
- core-WASM actors and synchronous key policy, plus Component Model and Preview
  1 actor/worker runtimes with completion I/O and notification coordination
- DBus control, status, agent, extension, and notification surfaces
- nested Manifold-in-Manifold behavior tests

## Design Bias

Manifold is local-first and power-user-oriented. Installed plugins are trusted
session code, so normal file, process, and network work should not trigger
repeated permission prompts. Effects should instead be observable, debuggable,
and reversible where practical. Direct local sessions have normal user
authority; containerized and remote sessions add the clear environment boundary
when isolation is required.

## Where To Read Next

- [[workflows/run-manifold]] if you want to run it.
- [[workflows/test-nested]] if you want to test behavior.
- [[systems/runtime]] if you want to understand process startup.
- [[systems/extension-host]] if you want to understand the current WASM and extension surface.
- [[systems/plugin-runtime]] if you want the target plugin architecture.
