---
status: current
type: reference
authority: implementation
last-reviewed: 2026-07-22
code-links:
  - crates/brood-wasm/src/module.rs
  - crates/brood-wasm/src/module_link.rs
  - crates/brood-wasm/src/actor.rs
  - crates/brood-wasm/src/actor_link.rs
  - crates/brood-wasm/src/global_key_policy.rs
  - crates/brood-wasm/src/global_key_policy_link.rs
  - crates/brood-wasm/src/resolved_layout.rs
  - crates/brood-wasm/src/policy_provider.rs
  - crates/brood-wasm/src/test_harness.rs
  - clients/brood-client/src/layout.rs
  - clients/brood-client/src/main.rs
  - crates/broodd/src/wasm_test.rs
  - crates/broodd/src/control.rs
  - crates/broodd/src/state.rs
---

# WASM Core-Module Lanes

The current core-WASM surface supports named actors, synchronous policy
providers, and the behavior-test harness. These are implementation lanes, not
the complete plugin SDK.

The target plugin model keeps the useful synchronous property for decisions
that must complete before host processing can continue. Policy providers read
immutable published state and return bounded decision envelopes plus message
batches; ordinary plugin work stays in actors and workers. See
[[../systems/plugin-runtime]], [[../systems/input-shortcuts]], and
[[../systems/layout-actor]].

## Core-Module Actors

Core plugins declare actors and typed event subscriptions through named exports
discovered by `module_link.rs`. Each actor processes one bounded mailbox epoch
and returns an outbox containing messages and publication frames. Presentation
actors and layout run only in the presentation client. Marker typos and
subscriptions without a declared actor reject the module; consult
`module_link.rs` for exact export spellings.

See `module.rs`, `module_link.rs`, `actor.rs`, and `actor_link.rs` for the exact
export and outbox ABI.

## Presentation Key Policy

The presentation client selects a named key policy provider from the module
inventory. It passes a serialized key query plus an immutable publication of
the client-owned pane inventory, local focus, and managed application
identities. The provider returns `consume`, `forward`, or `abstain` semantics
and may include a bounded batch of actor messages. The client validates and
applies the envelope; the provider does not perform host side effects directly.
Backend commands leave the synchronous lane through a bounded asynchronous
worker. The daemon does not host presentation key policy.

See `global_key_policy.rs`, `global_key_policy_link.rs`, and
`policy_provider.rs` for the exact export, descriptor, bounds, and decision
encoding.

## Presentation Layout Actors

A core-module actor declares the layout-state subscription through normal actor
metadata. The presentation host enqueues client-visible pane inventory, local
focus, application identities, and viewport size into that actor's mailbox.
Commands such as `layout-action` use the same addressed message route. The
actor submits one lease-local candidate only by addressing a
`set-resolved-layout` message to the ordinary host mailbox. The client composes
the current recursive lease tree into its final resolved layout.

The client validates each candidate before composition and hit testing. An
invalid result selects the client-local fallback. The client sends
requested managed-pane sizes to the daemon after accepting a layout; it never
sends placement. See `actor.rs`, `legacy_runtime.rs`, `resolved_layout.rs`, and
the client actor runtime for the exact mailbox and validation path.

## Async Test Harness

Export:

```text
run() -> i32
```

Return codes:

- `0`: pass
- `1`: fail
- `2`: pause for interactive inspection

Selected imports include `sleep_ms`, `log`, `status_json`, `control_json`, and `wait_for_signal`.

## Hot Replacement

Plugin replacement accepts a self-contained plugin artifact or an explicitly
selected key-policy artifact. Component Model and marked Preview 1 artifacts
are routed by the same daemon replacement entry point. See `control.rs` and
`state.rs` for exact parser and artifact-selection behavior.

Related:

- [[../systems/extension-host]]
- [[../systems/plugin-runtime]]
- [[../systems/input-shortcuts]]
- [[../systems/layout-actor]]
- [[../workflows/build-wasm-examples]]
