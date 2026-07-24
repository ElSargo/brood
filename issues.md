# Structural lint issues

These are lint findings that point at design debt rather than a local cleanup.
The current code carries TODO-backed `allow` annotations so CI can enforce new
warnings without pretending these areas are already structurally clean.

## Resolved

- Core-module actor generations now own their declarations, mailbox identities,
  cancellation signal, and join handles. Replacement builds every actor against
  unpublished mailboxes, validates the complete candidate set, cancels and joins
  the retiring generation, and atomically publishes the prepared set before new
  tasks run; invalid candidates leave the old names and actors reachable. Typed
  `pane-mapped` and `layout-state` marker exports are validated and routed only
  to declared subscribers, while direct actor messages share the public actor
  namespace. Actor replacement tests reject a deliberately broken second actor
  while a mailbox probe proves the old generation remains responsive, then
  hot-reload the same actor names. The runtime records both actors joined at
  replacement and final shutdown. Event marker
  subscriptions currently describe core-module actors; Component and Preview 1
  actors remain message-driven until their manifests gain an event-declaration
  surface.
- Core-WASM test harnesses now run as owned, checked child threads with typed
  `Starting -> Running -> Exited` events and explicit joined, already-joined,
  and panicked cleanup results. Signal, sleep, and host-response waits observe
  cancellation; Wasmtime epoch interruption stops executing guest code; gates
  before instantiation and `run()` close early-cancellation races; and a lost
  lifecycle channel becomes a failed completion. Ordered daemon shutdown
  records the cleanup outcome before the Tokio runtime is dropped, while the
  destructor remains a hard-join fallback. Harness lifecycle tests cover one
  child receiving a named signal, performing a final host request, returning,
  and joining without cancellation. Another stays blocked in the signal import
  until shutdown wakes, interrupts, and joins the exact child without inventing
  a guest completion. Native module
  compilation is joined rather than forcibly preempted mid-compilation.
- The named `wasm_key.rs` premise was stale after the prototype moved into the
  policy-provider runtime, but the remaining invocation boundary is now fully
  typed: `GlobalKeyPolicyInput` groups code, press/release state, modifiers, and
  input mode, while `GlobalKeyPolicyInvocation` couples that event envelope to
  its immutable publication lease. Input routing passes one context, guest
  allocation details are private, and only the core-WASM link adapter lowers
  bounded buffers to the four pointer/length scalars required by the stable
  guest ABI. `tools/test-typed-key-policy-nested.sh` routes to the full
  client-realm proof with the real `default-workspace` plugin and real key
  injection: `Ctrl+O` visibly opens exactly one second Foot terminal,
  `Ctrl+J` / `Ctrl+K` change managed-window focus, and `Ctrl+Alt+M` focuses,
  hides, and restores the session-switcher pane. Captured compositor frames and
  matching client and server state make the result observable.
- Session-local DBus routing now separates the audited bounded-wire layer from
  a typed `BusRouter`. Connection allocation, unique and well-known ownership,
  replacement flags, release, disconnect cleanup, route decisions, delivery
  failures, notification proxy results, and matching notification signals are
  explicit fallible transitions; built-in daemon and notification owners have
  stable unique identities and rejected requests leave owner tables unchanged.
  The real proof in `tools/test-typed-session-bus-nested.sh` renders the GTK
  session switcher with two live realms, observes different ordinary
  `DBUS_SESSION_BUS_ADDRESS` sockets and owner visibility, and drives four real
  Python DBus clients through introspection, a property round trip, methods,
  replies, and two signal hops. The last client sends a substantive
  notification over its managed session bus, and the transcript records the
  real notification ID returned by the host desktop service.
- Unsafe frame export, SHM, DBus-wire, and calloop access now sits behind narrow
  audited helpers with typed owners and documented lifetime, alignment, and
  validity invariants. EGL/GBM handles cannot escape the frame exporter;
  Smithay display dispatch is one state-layer helper; SHM upload
  validates dimensions, offset, stride, mapped span, complete pixels, and alpha
  policy before slice-only conversion; and DBus frames validate endian,
  alignment, exact length, checked arithmetic, and a 128 MiB ceiling before the
  two private zbus raw constructors. Broad unsafe/index allowances are gone,
  with malformed, short, zero-size/stride, overflow, padded-row, and alpha
  fixtures covering non-observable seams. External-client graphical checks
  cover the visible upload path; deterministic fixtures cover transient
  Smithay buffer-assignment boundaries.
- The launch-context issue's preview premise was stale: repository history
  confirms launch records have always been lossless transport data and no
  environment-preview bound or truncation function exists. The real
  `string_slice` debt was cgroup scope parsing; it now matches an exact,
  nonempty path component with structural prefix/suffix operations, preserving
  arbitrary Unicode without byte indexing. Tests cover malformed delimiters,
  long Japanese/combining/ZWJ/skin-tone/flag identifiers, and complete
  `PathBuf`/`OsString` record round trips. A plugin is not applicable before the
  session/plugin runtime starts, so `tools/test-unicode-launch-context.sh`
  provides the real proof: an actual user systemd scope and
  `manifold-runner-exec` restore full Japanese/emoji project paths,
  combining-mark/flag working paths, Cyrillic/emoji cache paths, and a
  CJK/combining/skin-tone/flag argument into both the process environment and
  cwd. Any future bounded UI preview must define its own bound and truncate at
  extended-grapheme boundaries while retaining the record as source of truth.
- Normalized plugin transforms, signed layout pixels, unsigned host/capture
  extents, physical pointer positions, and Wayland axis values now cross one
  checked geometry layer with explicit rounding, saturation, clamping, empty-
  size, invalid-scale, non-finite, and wheel-v120 policies. The old broad cast
  allowances are gone, capture dimensions are fallible, hit rectangles and
  split arithmetic cannot overflow, and boundary tests cover fractional values,
  the full signed range, NaN/infinities, zero and tiny scales, and host-size
  overflow. The current `workspace` actor receives lease-scoped state through
  its mailbox and emits its local fragment through the ordinary host outbox.
  External-client unit and graphical tests cover transform projection, clipping,
  hit testing, and managed-window resize convergence.
- Smithay protocol wiring and product runtime startup live in separate
  compositor and runtime modules with a narrow runtime-tick callback.
  Hosted spawns return an exact token with bounded lifecycle history, so the
  supervisor can wait either for that pane to be presented or, explicitly, for
  that process to exit; unrelated windows and child exits satisfy neither
  policy, and a known failure cannot be hidden by visibility.
- `Manifold` now coordinates typed surface-session, focus, registry, and test-
  harness services instead of owning those transition tables as loose fields.
  Session
  allocation, activation, routing, rename, directory assignment, and removal
  are atomic and fallible, including explicit empty-catalogue and exhausted-ID
  behavior. Agent notification requests are typed, harness ownership is single-
  assignment, and an absent async runtime is an error rather than a registry
  panic. Session-service tests cover switching, removal, directory-default
  retirement, and agent notification transitions.
- Raw argv and JSON control requests now cross typed `BrowserControlCommand`
  and `PlugCommand` parsing boundaries before compositor
  mutation. Exact arity and ambiguous browser URL forms are rejected without
  token indexing, and the obsolete `indexing_slicing` allowances are gone. The
  parser tests cover typed `plug --kind/--id/path` registration and exact
  browser diagnostics.
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
  policy, so the user-facing proof runs the real CLI: an entered command prints
  its domain rejection and exits 37, and the invoking shell observes
  `manifold enter` exit status 37.
- Subprocess pipes, real PTYs, loopback sockets, and timers now share one
  bounded byte-stream and notification/pollable lifecycle owned by the actor
  generation. Component and Preview 1 guests use the same capability grants,
  PATH-selected process resolution, caller working directory, exit status,
  backpressure, async drop, and structured teardown semantics; admission races,
  cancellation, stale handles, resource churn, and blocked host work have
  deterministic coverage. The real
  Component `component-runtime-io` plugin and the Preview 1
  `workspace-file-log` worker write separate visible transcripts: both feed an allowlisted `tr`
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
  exact races have deterministic gated tests, including two-generation hot
  replacement and structured shutdown with worker values 90 and 81.
- Preview 1 completion dispatch has its own generously sized bounded lane. The
  parent stops receiving from the completion supervisor while that lane is full,
  but continues servicing control and shutdown commands; completion, mailbox,
  and notification dispatch remain fair. The observable proof is the
  `workspace-file-log` plugin's `snapshot` action: actor completion I/O reads the
  live watch log and writes an exact snapshot while the watcher worker stays
  active. Runtime tests validate both real filesystem artifacts and the
  delivered completion count. This proof also
  caught and fixed a scheduler bug which misrouted completion-only work toward
  an empty notification lane.
- Wasm stores and actor message paths are byte-bounded. Shared, deliberately
  generous limits cover mailbox senders and bodies, Component worker payloads
  and results, generation-owned host queues, aggregate linear memory, and store
  tables. Byte permits follow retained values until dispatch or observation,
  and Preview 1 modules must declare shared-memory maxima within the generation
  ceiling. The target contract is documented in
  `docs/systems/plugin-runtime.md`. The observable proof is
  `plugins/workspace-file-log`: its acceptance workload performs a real
  workspace scan and records file creation, modification, and removal under the
  bounded runtime.
