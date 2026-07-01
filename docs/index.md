---
status: canonical
type: index
authority: documentation
last-reviewed: 2026-07-01
---

# Manifold Knowledge Base

This directory is organized as a wiki-style knowledge base. Pages are grouped by reader intent and marked with `status` frontmatter so current behavior, target design, experiments, and historical notes are not confused.

## Start Here

- [[overview]] explains what Manifold is.
- [[architecture]] maps the current implementation and intended boundaries.
- [[CONTEXT]] is the glossary and terminology source of truth.

## Current Behavior

- [[systems/runtime]] describes process startup, presentation attach, config loading, and session runners.
- [[systems/configuration]] describes `manifold.toml`, startup apps, modules, and config discovery.
- [[systems/testing]] describes the supported behavior-test and smoke-test paths.
- [[systems/dbus-control]] describes the DBus control surfaces.
- [[systems/wayland-compositor]] describes the nested Wayland compositor path.
- [[systems/extension-host]] describes current WASM and extension lanes.

## Core Concepts

- [[concepts/workspace]]
- [[concepts/session]]
- [[concepts/pane]]
- [[concepts/presentation-surface]]
- [[concepts/managed-app]]
- [[concepts/session-runner]]
- [[concepts/wasm-controller]]

## Workflows

- [[workflows/run-manifold]]
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
