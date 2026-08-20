---
status: canonical
type: index
authority: documentation
last-reviewed: 2026-08-21
---

# Brood Knowledge Base

This directory is organized by the role a document plays. Target design,
implementation history, and unresolved design friction are kept in separate
directories so implementation work does not silently rewrite product goals.

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
- [[systems/picker-actor]] describes the implemented reusable picker actor.

## Target Design

- [[targets/README]] indexes accepted product and architecture targets.
- Target pages describe the desired state even when implementation is behind.
- Active proposals remain in [[experiments/README]] until design work accepts
  them as targets.

## Core Concepts

- [[concepts/workspace]]
- [[concepts/session]]
- [[concepts/pane]]
- [[concepts/presentation-surface]]
- [[concepts/managed-app]]
- [[concepts/session-runner]]
- [[targets/concepts/plugin]]
- [[concepts/wasm-controller]]

## Workflows

- [[workflows/run-brood]]
- [[workflows/ssh-deploy]]
- [[workflows/container-backends]]
- [[workflows/enter-session]]
- [[workflows/test-nested]]
- [[workflows/develop-with-nix]]
- [[workflows/build-wasm-examples]]

## Working Records

- [[progress/README]] contains implementation milestones and evidence.
- [[frictions/README]] contains unresolved design-level mismatches and
  oversights.

## Reference

- [[reference/cli]]
- [[reference/config-schema]]
- [[reference/dbus-interfaces]]
- [[reference/wasm-fast-lanes]]

## Other Areas

- [[experiments/README]] contains active design experiments.
- [[archive/README]] contains historical design notes and older prototypes.
- [[decisions/README]] contains decision records.

## Status Values

- `canonical`: durable product or documentation source of truth.
- `current`: describes checked-in implementation behavior.
- `target`: describes intended direction that may not exist yet.
- `experiment`: describes active exploration.
- `progress`: records implementation history or verification evidence.
- `friction`: records unresolved input for design work.
- `archive`: historical context, not authoritative for current behavior.
