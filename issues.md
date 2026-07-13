# Structural lint issues

These are lint findings that point at design debt rather than a local cleanup.
The current code carries TODO-backed `allow` annotations so CI can enforce new
warnings without pretending these areas are already structurally clean.

## Plugin I/O follow-ups exposed by the real workspace watcher

- Native capability-rooted filesystem watches currently exist only in the
  Preview 1 compatibility ABI. The Component backend still needs an equivalent
  WIT resource and SDK wrapper before both plugin backends have one I/O model.
- Preview 1 watch batches currently encode paths as lossy UTF-8 text. The
  resource is capability safe, but a typed SDK/Component surface must preserve
  arbitrary platform path bytes without collisions.
- Native watcher backends may coalesce, duplicate, or classify rename events
  differently. Overflow is safely reported as `rescan`, but consumers that
  maintain state must implement reconciliation; the example logger records the
  marker rather than rebuilding an indexed model.
- A configured workspace filesystem grant captures the active session working
  directory when the plugin loads. It is not retargeted when another surface
  session becomes active. Session-scoped plugin instantiation or an explicit
  rebind lifecycle is still needed.
- `startup-message` is an untyped bootstrap convenience, not a complete plugin
  lifecycle interface. Manifest-level start/stop hooks and structured startup
  data remain to be designed.
- Watch and event-queue limits currently use runtime defaults. They are bounded,
  but plugin manifests cannot yet request narrower budgets and status output
  does not expose live watch counts or overflow totals.
- Subprocesses, pipe streams, PTYs, sockets, and timers remain deliberately
  absent. They should reuse the same generation-owned resource and pollable
  lifecycle rather than grow separate wait mechanisms.

## Control and DBus API shape

- `crates/manifoldd/src/control.rs`: `exit` in the command path shows that CLI
  process policy is still mixed into control logic. A proper fix should return
  typed exit outcomes from command execution and let the binary entrypoint exit.
- `crates/manifoldd/src/bin/manifold-runner-nix-develop.rs`: the runner must
  preserve the activated child process exit status. A proper fix should move the
  binary entrypoint to an explicit exit-status return model instead of calling
  `process::exit` from helper code.
- `crates/manifoldd/src/control.rs`, `crates/manifoldd/src/browser.rs`: command
  handlers still parse token slices positionally, which drives
  `indexing_slicing` warnings. The target design should move these paths to
  typed parsers or typed command structs.

## Central runtime state

- `crates/manifoldd/src/state.rs`: the `Manifold` state object owns session
  routing, geometry, focus, agent registry, browser state, and test harness
  coordination. That drives `too_many_arguments`, pass-by-value churn,
  field-name repetition, unchecked indexing, and panic-like invariant checks.
  The target design should split this into typed services with explicit
  ownership and fallible boundaries.
- `crates/manifoldd/src/compositor.rs`: runtime setup and nested-test
  orchestration are interleaved with compositor wiring. The target design should
  split test orchestration from compositor startup and give event-loop callbacks
  narrow typed inputs.

## Geometry and conversion model

- `crates/manifoldd/src/layout.rs`, `crates/manifoldd/src/input.rs`,
  `crates/manifoldd/src/presentation.rs`, and
  `crates/manifoldd/src/winit_presenter.rs`: coordinate paths still mix float
  ratios, signed layout pixels, unsigned renderer sizes, and raw casts. A proper
  fix should introduce checked geometry conversion helpers and make truncation,
  clamping, and overflow policy explicit.
- `crates/manifoldd/src/launch_context.rs`: environment previews slice strings
  by byte offsets. The target design should define truncation in Unicode scalar
  or grapheme terms before changing behavior.

## Unsafe and raw buffers

- `crates/manifoldd/src/winit_presenter.rs`,
  `crates/manifoldd/src/debug_dump.rs`, `crates/manifoldd/src/session_bus.rs`,
  and `crates/manifoldd/src/state.rs`: unsafe blocks and raw buffer indexing sit
  at FFI, renderer, and DBus-message boundaries. The target design should wrap
  those boundaries in audited safe helpers with documented invariants rather
  than scattering safety comments around inline raw access.

## Session bus routing

- `crates/manifoldd/src/session_bus.rs`: the in-process DBus router operates on
  raw message bytes, manual owner maps, and callback tuples. That drives
  `type_complexity`, `unreachable`, pass-by-value, ignored-send-result, and
  indexing warnings. A proper fix should model session bus messages, owners, and
  notification forwarding as typed state transitions.

## WASM invocation and harness lifecycle

- `crates/manifoldd/src/wasm_key.rs`: key-handler calls pass event context as
  parallel scalar arguments. The target design should group those fields into a
  typed invocation context.
- `crates/manifoldd/src/wasm_test.rs`: harness process lifecycle is procedural
  and relies on best-effort child cleanup. The target design should model child
  handles, lifecycle events, and cleanup results explicitly.
- Target-shape WASM plugin actor loading is wired enough for interactive testing,
  but reload/lifecycle ownership is still provisional. Actor tasks currently run
  indefinitely without cancellation handles, reloading a module with the same
  actor names can fail registry registration, and pane-mapped events are
  broadcast to all WASM actors rather than subscription-routed.

## Resolved

- Manifold owns one native filesystem-watcher service which deduplicates
  canonical path plus recursion-mode registrations across actor generations.
  Logical subscriptions keep private bounded queues, overflow, notification,
  capability projection, and teardown; stale setup results cannot attach to a
  recreated registration and backends are never destroyed under the registry
  mutex. Status exposes native registrations, logical subscriptions, and setup
  tasks. The real workspace plugin proves the shape with two active workers and
  identical 198-byte logs backed by one registration and two subscriptions.
- Filesystem-watch setup tasks are keyed and reaped, while unfinished orphan
  setup still counts against the watch ceiling until it exits. Logical drop
  never waits for recursive registration. Host teardown retires a watch's
  notification before removing the watch, invalidates pollables, adjusts an
  admitted actor-event count exactly once, and lets any stale queued ID drain as
  a no-op. Unit tests cover 32 create/drop cycles and queued-event retirement;
  the real workspace plugin repeats 32 guest teardown probes, returns to one
  primary notification, and then visibly records `after-watch-churn.txt`.
- Completion-I/O cancellation and replacement own blocking writes through their
  terminal boundary. Cancellation can prevent queued work from starting but
  does not roll back an already-started filesystem effect; once a write has
  started, its blocking task is joined before `cancelled` is delivered. Shutdown
  likewise drains admitted writes before replacement can activate, so an old
  generation cannot mutate files afterward. Deterministic blocked-mutation
  tests prove that ordering, while `workspace-file-log` leaves a real
  completion-written snapshot as the user-observable guest-to-daemon proof.
- Preview 1 shutdown treats queued and in-flight worker preparation as owned
  lifecycle state. Stopping cancels queued preparations, waits for late Stores
  to return, drops them before execution, releases every guest stack/TLS
  reservation, and reports cancellation to host-owned spawns. Already-queued
  reservation releases are preserved while other actor work is discarded. The
  exact races have deterministic gated tests; the real
  `tools/test-wasm-preview1-worker-nested.sh` workload proves two-generation hot
  replacement and structured shutdown with worker values 90 and 81.
- Preview 1 completion dispatch has its own generously sized bounded lane. The
  parent stops receiving from the completion supervisor while that lane is full,
  but continues servicing control and shutdown commands; completion, mailbox,
  and notification dispatch remain fair. The observable proof is the
  `workspace-file-log` plugin's `snapshot` action: actor completion I/O reads the
  live watch log and writes an exact snapshot while the watcher worker stays
  active. `tools/test-workspace-file-log-plugin-nested.sh` validates both real
  filesystem artifacts and the delivered completion count. This proof also
  caught and fixed a scheduler bug which misrouted completion-only work toward
  an empty notification lane.
- Wasm stores and actor message paths are byte-bounded. Shared, deliberately
  generous limits cover mailbox senders and bodies, Component worker payloads
  and results, generation-owned host queues, aggregate linear memory, and store
  tables. Byte permits follow retained values until dispatch or observation,
  and Preview 1 modules must declare shared-memory maxima within the generation
  ceiling. The target contract is documented in
  `docs/systems/plugin-runtime.md`. The observable proof is
  `plugins/workspace-file-log`: its nested test performs a real workspace scan
  and records file creation, modification, and removal under the bounded
  runtime; see `tools/test-workspace-file-log-plugin-nested.sh`.
