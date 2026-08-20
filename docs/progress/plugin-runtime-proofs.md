---
status: progress
type: verification-record
authority: implementation
---

# Plugin Runtime Proofs

The Component and Preview 1 runtimes provide implementation proofs for
[[../targets/systems/plugin-runtime]]. Their export names and Wasmtime Store
layout are implementation details, not the durable plugin ABI.

The bundled `terminal-session` and `terminal-ui` components prove the split
terminal product boundary. The session guest launches an allowlisted shell in a
PTY, parses escape sequences, retains grid and scrollback state, and publishes
coalesced revisions. The presentation guest handles real pane input and resize,
renders styled native text through canvas packets, and can be replaced without
terminating the session-owned shell. The nested workload records shell,
ANSI/Unicode, key bytes, Helix, alternate-screen resize, scrollback, streaming
output, child exit, resource pressure, mailbox, packet, text churn,
key-to-present, teardown, and screenshot evidence.

`plugins/component-runtime-io` and `plugins/workspace-file-log` exercise the
bounded stream and pollable contract through both backends. Their workload
records PATH-selected commands, pipes, nonzero child exit, PTY traffic, a real
loopback exchange, and ordered timers in separate guest-visible transcripts.

`plugins/workspace-file-log` also proves bounded shared memory, recursive
workspace scanning, live filesystem watches, overflow recovery, and reversible
non-UTF-8 path encoding. `plugins/component-workspace-watch` proves the typed
Component resource over the same daemon watcher, including replacement cleanup
before publication of the next actor generation.

Host-owned teardown retires each watch and notification as one ownership
transition. Already-admitted stale events drain as no-ops. Keyed recursive
setup tasks remain within the watch ceiling until reaped.

Current implementation maps and acceptance entry points are in
[[../systems/extension-host]], [[../systems/plugin-io-interaction-graph]],
[[../reference/wasm-fast-lanes]], and [[../systems/testing]].
