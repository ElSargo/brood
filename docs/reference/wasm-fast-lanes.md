---
status: current
type: reference
authority: implementation
last-reviewed: 2026-07-11
code-links:
  - crates/manifold-wasm/src/module.rs
  - crates/manifold-wasm/src/module_link.rs
  - crates/manifold-wasm/src/actor.rs
  - crates/manifold-wasm/src/actor_link.rs
  - crates/manifold-wasm/src/global_key_policy.rs
  - crates/manifold-wasm/src/global_key_policy_link.rs
  - crates/manifold-wasm/src/policy_provider.rs
  - crates/manifoldd/src/wasm_test.rs
  - crates/manifoldd/src/control.rs
  - crates/manifoldd/src/state.rs
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
[[../systems/layout-policy]].

## Core-Module Actors

Core plugins declare actors and typed event subscriptions through named exports
discovered by `module_link.rs`. Each actor processes one bounded mailbox epoch
and returns an outbox containing messages and publication frames. Layout policy
uses this path: the host routes pane/layout state only to subscribing actors,
and an actor sends a validated `set-resolved-layout` command back through the
host mailbox. Marker typos and subscriptions without a declared actor reject
the module; consult `module_link.rs` for exact export spellings.

See `module.rs`, `module_link.rs`, `actor.rs`, and `actor_link.rs` for the exact
export and outbox ABI.

## Global Key Policy

The daemon selects a named key policy provider from the module inventory. It
passes a serialized key query plus the latest immutable actor publication. The
provider returns `consume`, `forward`, or `abstain` semantics and may include a
bounded batch of actor messages. The host validates and applies the envelope;
the provider does not perform host side effects directly.

See `global_key_policy.rs`, `global_key_policy_link.rs`, and
`policy_provider.rs` for the exact export, descriptor, bounds, and decision
encoding.

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
- [[../systems/layout-policy]]
- [[../workflows/build-wasm-examples]]
