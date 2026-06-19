# WASM Test Harness Prototype

The first async test-harness prototype is available through:

```sh
manifoldd run --test-harness /path/to/harness.wasm
```

This is intentionally separate from `--wasm-key-handler`.

- `--wasm-key-handler` is a synchronous fast-lane hook for key handling.
- `--test-harness` is an async controller lane for behavior testing.

## Current Guest ABI

The guest must export:

```text
run() -> i32
```

Result codes:

- `0`: pass
- `1`: fail
- `2`: pause for interactive inspection

The host imports these functions from module `manifold`:

```text
sleep_ms(millis: i32)
log(ptr: i32, len: i32) -> i32
status_json(resp_ptr: i32, resp_cap: i32) -> i32
control_json(req_ptr: i32, req_len: i32, resp_ptr: i32, resp_cap: i32) -> i32
wait_for_signal(resp_ptr: i32, resp_cap: i32) -> i32
```

`status_json` writes the current `status-json` payload into guest memory and returns the number of bytes written, or `-1` on failure.

`control_json` accepts the same JSON argv array accepted by `manifoldd ctl --json`, writes the textual command response into guest memory, and returns the number of bytes written, or `-1` on failure.

`wait_for_signal` blocks the harness thread until the host sends a named signal through:

```sh
manifoldd ctl harness signal proceed
```

The signal name is written into guest memory and the function returns the number of bytes written, or `-1` on failure.

## Runtime Shape

The test harness runs on a separate thread with its own Wasmtime store. It does not execute on the calloop/session thread.

Requests cross back into the session through a bounded host-owned request path:

```text
test harness thread
  -> command/status request
  -> calloop/session thread services request
  -> textual/JSON response
```

This keeps `Manifold` state authoritative on the session thread while still allowing the harness to block synchronously on host responses from its own thread.

## Scope Of This Prototype

This is a bootstrap lane, not the final async module runtime.

Implemented:

- separate WASM harness lane
- in-process reuse of the existing control surface
- pass/fail/pause completion reporting

Not implemented yet:

- event subscriptions
- typed command/result model
- screenshot capture
- artifact bundles
- hot reload
- multiple concurrent harness modules
