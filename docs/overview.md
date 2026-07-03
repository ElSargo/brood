---
status: canonical
type: overview
authority: product
last-reviewed: 2026-07-03
---

# Overview

Manifold is a local persistent graphical workspace for development. It explores whether an extra compositor/session layer can feel native while preserving app and agent state across presentation attach, detach, and switching.

The product model is:

- A [[concepts/workspace]] contains one or more [[concepts/session]]s.
- A session groups panes, programs, agents, environment, cwd, and capabilities.
- A [[concepts/presentation-surface]] shows one session at a time.
- A [[concepts/pane]] is the unit of presentation.
- A [[concepts/managed-app]] is an external native app tracked by Manifold.
- A [[concepts/plugin]] can extend behavior through host-owned mailboxes, sync policy lanes, swapchains, and capability-checked native APIs.
- A [[concepts/wasm-controller]] is one plugin role for changing workspace behavior through host APIs.

The current implementation is a single Rust package named `manifoldd` that builds the user-facing `manifold` binary. That binary currently owns the session process, nested Wayland compositor, DBus control endpoint, and MVP presentation endpoint.

See [[architecture]] for the implementation map.

## Current Focus

The repository is currently proving:

- nested Wayland app hosting
- native presentation attach/detach
- keyboard and pointer forwarding
- session switching
- startup modules and startup apps from config
- prototype fast WASM key-handler and map-hook lanes that inform the target [[systems/plugin-runtime]] sync policy model
- DBus control, status, agent, extension, and notification surfaces
- nested Manifold-in-Manifold behavior tests

## Design Bias

Manifold is local-first and power-user-oriented. Explicit user/session actions should work without confirmation prompts by default, but effects should be observable, debuggable, and reversible where practical.

## Where To Read Next

- [[workflows/run-manifold]] if you want to run it.
- [[workflows/test-nested]] if you want to test behavior.
- [[systems/runtime]] if you want to understand process startup.
- [[systems/extension-host]] if you want to understand the current WASM and extension surface.
- [[systems/plugin-runtime]] if you want the target plugin architecture.
