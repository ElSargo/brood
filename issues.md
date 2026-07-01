# Structural lint issues

These are lint findings that point at design debt rather than a local cleanup.
The current code carries TODO-backed `allow` annotations so CI can enforce new
warnings without pretending these areas are already structurally clean.

## Control and DBus API shape

- `crates/manifoldd/src/control.rs`: `unused_async` is concentrated in DBus
  interface methods. Removing it properly means changing how the zbus-facing API
  is exposed, not just deleting keywords.
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
