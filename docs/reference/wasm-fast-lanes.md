---
status: current
type: reference
authority: implementation
last-reviewed: 2026-07-03
code-links:
  - crates/manifoldd/src/wasm_key.rs
  - crates/manifoldd/src/wasm_map.rs
  - crates/manifoldd/src/wasm_test.rs
---

# WASM Fast Lanes

The current runnable WASM surface is a set of narrow core-WASM lanes. These are implementation lanes, not the final product API.

The target plugin model keeps the useful synchronous property of these lanes but replaces direct side-effect-style host imports with policy providers that read immutable published state and return bounded decision envelopes plus message batches. See [[../systems/plugin-runtime]], [[../systems/input-shortcuts]], and [[../systems/layout-policy]].

## Fast Key Handler

Preferred export:

```text
on_key_v2(key_code: i32, key_state: i32, input_mode: i32, ctrl_pressed: i32, modifiers: i32) -> i32
```

Compatibility export:

```text
on_key(key_code: i32, key_state: i32, input_mode: i32, ctrl_pressed: i32) -> i32
```

Modifier bits:

- `1`: Ctrl
- `2`: Alt

Selected imports include layout actions, pane lookup, exact pane focus, spawn, session switching, and session creation. See `crates/manifoldd/src/wasm_key.rs`.

Those imports currently append pending host commands while the synchronous call is active. In the target model, the key lane returns `Consume`, `Forward`, or `Abstain` plus host-validated directives/messages after reading its latest state frame.

## Fast Map Hook

Export:

```text
on_map_v1(pending: i64, visible_count: i32) -> i32
```

Selected imports:

- `visible_pane_id(index: i32) -> i64`
- `set_map_response(ptr: i32, len: i32) -> i32`

Responses are JSON and are validated by the host before affecting presentation.

This is the current prototype of the target layout policy provider. The target response should become a typed layout decision envelope backed by immutable published state, with the host still owning validation, fallback, hit testing, focus, and rendering.

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

`manifold plug <path>` replaces a running WASM lane. `--kind auto` tries fast key handler first, then fast map hook.

Related:

- [[../systems/extension-host]]
- [[../systems/plugin-runtime]]
- [[../systems/input-shortcuts]]
- [[../systems/layout-policy]]
- [[../workflows/build-wasm-examples]]
