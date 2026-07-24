---
status: canonical
type: overview
authority: product
last-reviewed: 2026-07-24
---

# Overview

Manifold is a local persistent graphical workspace for development. It explores
whether an extra compositor/session layer can feel native while preserving app
and agent state across frontend replacement and session switching.

The product model is:

- A [[concepts/workspace]] contains one or more [[concepts/session]]s.
- A session groups panes, programs, agents, environment, cwd, and capabilities.
- A [[concepts/presentation-surface]] is an independent client viewport that shows one session at a time.
- A [[concepts/pane]] is the unit of presentation.
- A [[concepts/managed-app]] is an external native app tracked by Manifold.
- A [[concepts/plugin]] can extend behavior through addressed actors, private actor-scoped workers, completion-based I/O, WASI resources, policy providers, and publications.
- A [[concepts/wasm-controller]] is one plugin role for changing workspace behavior through host APIs.

The Rust workspace contains an authoritative session server and an independent
portable presentation client. The server owns the nested Wayland compositor,
session state, hosted applications, durable actors, and DBus bootstrap. The
client owns presentation actors, recursive layout leases, input policy, and
WGPU rendering. Local clients use shared-memory protocol lanes plus DMA-BUF or
copied per-window surfaces; remote clients retain the same client-owned
viewport over an authenticated carrier. See [[systems/presentation-client]].

See [[architecture]] for the implementation map.

## Current Focus

The repository is currently proving:

- nested Wayland app hosting
- external presentation-client reconnect without hosted-app loss
- recursive client-local layouts and presentation plugins
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
