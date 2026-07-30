---
status: canonical
type: index
authority: documentation
last-reviewed: 2026-07-24
---

# Brood Knowledge Base

This directory is organized as a wiki-style knowledge base. Pages are grouped by reader intent and marked with `status` frontmatter so current behavior, target design, experiments, and historical notes are not confused.

## Start Here

- [[overview]] explains what Brood is.
- [[architecture]] maps the current implementation and intended boundaries.
- [[CONTEXT]] is the glossary and terminology source of truth.

## Current Behavior

- [[systems/runtime]] describes server/client startup, config loading, and session runners.
- [[systems/configuration]] describes `brood.toml`, startup apps, modules, and config discovery.
- [[systems/testing]] describes the supported real-workload acceptance paths.
- [[systems/dbus-control]] describes the DBus control surfaces.
- [[systems/wayland-compositor]] describes the nested Wayland compositor path.
- [[systems/extension-host]] describes current WASM and extension lanes.

## Target Design

- [[systems/presentation-client]] specifies the independent portable presentation client, session lease, window frame subscriptions, client-side WASM runtime, recursive layout, WGPU renderer, and transports.
- [[systems/presentation-transport]] defines the transport-neutral protocol,
  capability-selected local and network carriers, and frame-resource profiles.
- [[systems/artifact-distribution]] defines symmetric, content-addressed WASM
  hydration between client and session realms.
- [[systems/plugin-runtime]] defines plugin trust, actors, actor-scoped workers,
  policy providers, publications, and session isolation.
- [[systems/plugin-io-and-resources]] specifies completions, notifications,
  worker select, external resources, scopes, shutdown, and diagnostics.
- [[systems/input-shortcuts]] describes host-owned shortcut routing with privileged sync key policy.
- [[systems/layout-actor]] describes actor-driven layout updates and host-validated resolved layouts.
- [[systems/picker-actor]] defines the reusable queued fuzzy picker actor and its mailbox protocol.
- [[systems/plugin-ui]] describes guest-side SDK components, pane input, native chrome, and canvas rendering.
- [[systems/plugin-graphics]] specifies the base canvas draw-packet, native text, resource, submission, and replay contract.
- [[systems/plugin-advanced-graphics]] records the separate future protocol direction for user geometry, shaders, pipelines, and other GPU-style facilities.
- [[systems/terminal-plugin]] defines the bundled first-party terminal product, its guest-owned architecture, and the reusable host gaps exposed by the workload.
- [[systems/editor-plugin]] defines the client-local Wasm Neovim editor, its
  remote backend file actor, and its bounded bidirectional actor-message
  bridge.
- [[systems/inbox-plugin]] defines the persistent client-global notification
  queue and click-to-source actor routing.

## Core Concepts

- [[concepts/workspace]]
- [[concepts/session]]
- [[concepts/pane]]
- [[concepts/presentation-surface]]
- [[concepts/managed-app]]
- [[concepts/session-runner]]
- [[concepts/plugin]]
- [[concepts/wasm-controller]]

## Workflows

- [[workflows/run-brood]]
- [[workflows/ssh-deploy]]
- [[workflows/enter-session]]
- [[workflows/test-nested]]
- [[workflows/develop-with-nix]]
- [[workflows/build-wasm-examples]]

## Reference

- [[reference/cli]]
- [[reference/config-schema]]
- [[reference/dbus-interfaces]]
- [[reference/wasm-fast-lanes]]

## Non-Canonical Areas

- [[experiments/README]] contains active design experiments.
- [[archive/README]] contains historical design notes and older prototypes.
- [[decisions/README]] contains decision records.

## Status Values

- `canonical`: durable product or documentation source of truth.
- `current`: describes checked-in implementation behavior.
- `target`: describes intended direction that may not exist yet.
- `experiment`: describes active exploration.
- `archive`: historical context, not authoritative for current behavior.
