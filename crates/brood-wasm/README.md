# Brood WASM Runtime

`brood-wasm` is the runtime boundary for hosted WASM code. It owns:

- Wasmtime engine and Store configuration;
- plugin artifact classification and preparation;
- legacy core-WASM actor generations;
- Component Model and Preview 1 actor/worker generations;
- lifecycle, mailbox, publication, completion-I/O, and resource-limit machinery;
- the Wasmtime-backed nested behavior-test harness engine.

Embedding hosts provide capabilities and adapt typed runtime events, requests,
and outcomes to their own state. They should not configure Wasmtime, inspect
modules, or implement actor execution and cancellation loops themselves.

The session daemon remains responsible for workspace/session authority,
configuration policy, compositor state, and translating runtime output into
host commands.
