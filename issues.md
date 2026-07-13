# Structural lint issues

These are lint findings that point at design debt rather than a local cleanup.
The current code carries TODO-backed `allow` annotations so CI can enforce new
warnings without pretending these areas are already structurally clean.

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

- Raw argv and JSON control requests now cross typed `BrowserControlCommand`,
  `PlugCommand`, and `InjectCommand` parsing boundaries before compositor
  mutation. Exact arity and ambiguous browser URL forms are rejected without
  token indexing, and the obsolete `indexing_slicing` allowances are gone. The
  real `wasm-component-actor` example proves the plugin-facing route through
  `tools/test-typed-control-parser-nested.sh`: a typed `plug --kind/--id/path`
  command visibly registers the requested Component actor and its mailbox
  counter advances, while the same artifact records JSON browser output, exact
  invalid browser/inject diagnostics, and an unchanged input-event count.
- The Nix development runner returns a typed `ActivatedChildExit` from its
  activation helper and leaves OS termination to the binary entrypoint. Numeric
  child codes are preserved and Unix signals follow the conventional
  `128 + signal` shell status. A plugin is not applicable beneath the plugin
  execution layer, so the real proof runs the actual runner through its cached
  `nix print-dev-env` activation: the inner workload visibly records its
  Nix-store Cargo path, project/working/cache context, and a deliberate
  `configuration-rejected` result before the invoking shell observes status 37.
- Session command execution returns a typed `CommandOutcome` instead of
  terminating from `control.rs`; only the `manifold` binary entrypoint maps an
  explicit child code to its process exit status, while signal termination
  remains a fallible control result. A plugin is not applicable to binary exit
  policy, so the user-facing proof runs the real CLI in the built-in offscreen
  nested harness: an entered command visibly prints its domain rejection and
  exits 37, and the invoking shell observes `manifold enter` exit status 37.
- Subprocess pipes, real PTYs, loopback sockets, and timers now share one
  bounded byte-stream and notification/pollable lifecycle owned by the actor
  generation. Component and Preview 1 guests use the same capability grants,
  PATH-selected process resolution, exit status, backpressure, async drop, and
  structured teardown semantics; admission races, cancellation, stale handles,
  resource churn, and blocked host work have deterministic coverage. The real
  Component `component-runtime-io` plugin and the Preview 1
  `workspace-file-log` worker write separate visible transcripts through
  `tools/test-runtime-io-plugins-nested.sh`: both feed an allowlisted `tr`
  process through guest-visible pipes, preserve exit 7, prove both ends of an
  interactive PTY are terminals, exchange distinct requests with a real
  controller-owned TCP peer whose independent log corroborates them, and
  observe the 20 ms timer before the 80 ms timer through the ordinary wait set.
- Hosted plugin manifests can narrow logical filesystem-watch and per-watch
  event ceilings through a validated resource table; zero, daemon-ceiling
  bypass, and policy loss across stable-ID replacement are covered by tests.
  The same limits apply to Component and Preview 1 generations and survive
  workspace rebind. Generation stats expose live handles, configured ceilings,
  overflows, and rescans, while the shared daemon watcher reports monotonic
  totals; initial setup counts only as rescan and queue/backend collapse counts
  as both. The real workspace plugin runs with 3 watches/64 events, keeps 2
  handles live, pauses one consumer through an 80-file paced workload, and
  records daemon and generation overflow changes from 0 to 1. Its visible
  85-line index exactly reflects the reconciled workspace and its preserved
  status JSON records the applied capacities and counters.
- Configured hosted actors have a typed, explicitly acknowledged lifecycle on
  both Component and Preview 1 backends. Manifest lifecycle tables provide
  sorted structured start data and separate nonzero start/stop bounds; legacy
  `startup-message` cannot be mixed with the target interface. Replacement
  constructs an unreachable candidate, completes the old stop hook, hard-joins
  old resources regardless of hook outcome, completes candidate start while it
  is still unreachable, and only then publishes it. Rebind and final shutdown
  use typed reasons and the same hard-teardown fallback. The real workspace
  plugin delegates each hook artifact append to ordinary WASI work and only
  acknowledges a zero-result worker. Its visible logs prove generations 1, 2,
  and 3 in ordered initial/rebind/shutdown transitions, include session, guest
  root, instance, and manifest data, and remain byte-stable after daemon exit.
- Configured workspace grants follow active surface-session lifecycle through
  an explicit stable-ID generation rebind. Every runtime activation path stops
  and joins workspace-granted actor generations before changing the active
  session, then starts new generations rooted at the target session cwd and
  redelivers startup; a failed restart leaves the actor unavailable instead of
  retaining old-workspace authority. The real workspace plugin proof activates
  two live sessions with distinct directories and observes generations 1, 2,
  and 3. Each directory receives only its own initial and active-time mutation
  records, while hashes prove the inactive log remains unchanged and the live
  control API confirms the expected session ID and working directory.
- The Preview 1 workspace plugin is a stateful watch consumer, not merely an
  event logger. It maintains a sorted, atomically replaced workspace index;
  ordinary create/remove batches update membership, while any batch containing
  `rescan` or a backend-generic `changed` event is discarded and rebuilt by a
  recursive WASI scan before reconciliation is reported complete. Its nested
  proof pauses the primary consumer with its watch live, routes 4,200 files
  through the independently draining mirror to exceed the 4,096-event bound,
  then resumes to an overflow rescan. The primary log contains no incremental
  overflow rows, and the resulting 4,204-path index exactly matches an
  independent walk of the final workspace, including additions, deletion, and
  documented exclusions.
- Filesystem-watch events preserve native path identity end to end: raw
  `OsStr` bytes on Unix and little-endian UTF-16 units on Windows. Component
  events expose opaque byte lists, while the line-oriented Preview 1 ABI uses
  reversible uppercase `%HH` escaping for non-graphic bytes, `%`, tabs, and
  newlines. Unit coverage includes delimiter bytes and two invalid-UTF-8 names
  that lossy conversion would conflate. The real Component plugin renders those
  names distinctly as `collision-\xFF.txt` and `collision-\xFE.txt`; the real
  Preview 1 plugin records `preview1-collision-%FF.txt` and `%FE.txt` distinctly
  in both watcher logs and the completion-created snapshot.
- Component plugins own capability-rooted filesystem watches through a typed
  WIT resource with typed event batches. The guest wrapper projects readiness
  into the ordinary notification/pollable wait path, while the host reuses the
  daemon-owned native watcher service and retires subscriptions before actor
  replacement completes. `plugins/component-workspace-watch` is the observable
  proof: its nested workload creates, edits, and removes real workspace files,
  and the plugin maintains `.manifold/component-workspace-watch.md` with the
  corresponding paths and event kinds. The same proof observes one live native
  registration and logical subscription, then zero of each after generation
  replacement.
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
