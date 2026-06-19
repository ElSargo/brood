# Apps

This page describes the target app model only.

Manifold apps are intended to be sandboxed, capability-scoped modules. WASM is the preferred direction for built-in and user-facing apps that do not need native GPU or process integration.

The canonical design document is [WASM Integration](wasm-integration.md). Use that page for:

- the target app model
- workspace and scene boundaries
- capability shape
- module roles
- milestones and non-goals

Use the narrower WASM pages only for focused design notes:

- [WASM Execution Model](wasm-execution-model.md)
- [WASM API Prototype](wasm-api-prototype.md)
- [WASM Test Harness Prototype](wasm-test-harness-prototype.md)
