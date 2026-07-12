# Structural lint issues

These are lint findings that point at design debt rather than a local cleanup.
The current code carries TODO-backed `allow` annotations so CI can enforce new
warnings without pretending these areas are already structurally clean.

## Plugin I/O follow-ups exposed by the real workspace watcher

- Wasm stores and actor message paths are not yet bounded by bytes. Mailbox
  bodies, sender strings, worker payloads, worker results, and aggregate guest
  memory can therefore exhaust daemon memory even though queue item counts are
  bounded. Address this before stabilizing the runtime by adding Wasmtime
  memory/table limits plus per-message, per-result, and per-generation byte
  budgets. These limits should be deliberately very generous: they are
  fault-containment ceilings for runaway plugins, not constraints that normal
  coding workloads should approach.
- The Preview 1 parent drains completion results into `actor_operations`
  without applying `max_pending_actor_operations` or a separate completion-lane
  bound. A completion handler which submits more operations can grow this queue
  indefinitely while the outstanding-operation count remains within its limit.
  Add a reserved bounded completion lane and stop draining the supervisor while
  it is full. As with byte budgets, use a very generous limit so ordinary bursty
  completion traffic is unaffected while unbounded growth is impossible.
- Preview 1 shutdown cancels workers which are already active, but a worker
  preparation which completes after shutdown begins can still start a new
  worker. That worker missed the cancellation pass and can delay or hang hot
  replacement. Once stopping begins, every late preparation result must drop
  its prepared Store, release its reservation, and report cancellation instead
  of starting the worker.
- Completion writes run through `spawn_blocking`, but cancellation and shutdown
  abort only the async wrapper. A blocking filesystem operation which has
  already started can continue mutating the filesystem after a cancellation
  completion or actor replacement. Track and join completion-engine blocking
  work as Component worker I/O already does, or explicitly weaken and document
  the cancellation contract for external side effects.
- Filesystem watch churn is not fully bounded. Each watch creation retains a
  setup-thread handle until the entire hub stops, including after that watch is
  dropped, so repeated create/drop cycles grow `setup_threads`. In addition,
  `drop_watch` removes the watch before dropping its notification; if a queued
  actor event makes `drop_notification` fail, the notification remains live
  after its owning watch is gone. Reap keyed setup tasks and make watch plus
  notification teardown atomic or safely deferred.
- Native watches currently create one `notify` backend per logical watch
  resource. Replace that with a daemon-wide watcher service which deduplicates
  native path registrations and routes events into bounded, actor-generation
  scoped subscription queues. Keep the existing guest watch/notification ABI;
  capability-relative path projection, overflow, and teardown remain private
  to each logical handle even when the kernel registration is shared.
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
