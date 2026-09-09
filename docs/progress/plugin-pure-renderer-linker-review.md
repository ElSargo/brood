---
status: progress
type: implementation-review
authority: implementation
last-updated: 2026-09-05
---

# Pure renderer linker review

## Current decision: accepted limitation

The user clarified the existing trusted-plugin model in [[../targets/systems/plugin-runtime]]
and explicitly accepted the risk of host argument allocation before dynamic
denial. The target intentionally records
that fault-containment model. Dynamic trap linkage is permitted for a separate
facet instance of the same compiled component. Normal guest memory/execution
limits, snapshot/result admission, resource accounting, and failed-context
retirement remain required.

Implementation resumes the same-artifact path. Correct sync/async import typing
and ordinary trap isolation are functional requirements. Separate artifacts and
pre-lifting adapters are no longer required to address this issue.

## Historical observation

In Wasmtime 46.0.1, dynamic host functions receive owned `Val` arguments after
Component Model lifting. The host trampoline loads parameters before invoking
the registered closure (`runtime/component/func/host.rs`, `call_sync_lower` and
`DynamicHostFn::lift_params`). Nested strings/lists can therefore allocate host
memory before a denial stub runs. Guest memory bounds alone do not meter those
host allocations. This remains true even if the stub immediately traps.

A modest diagnostic experiment used 32 references to one 4 KiB string in a
64 KiB guest memory. The stub observed 128 KiB of owned string payload before
returning its error. The assertion of this runtime limitation was removed from
the required test suite after the user accepted the risk; implementation should
not require the limitation to persist in future Wasmtime versions.

The initial rejection assumed stronger allocation isolation than the user wants.
Its proposed separate-artifact requirement is superseded. The built-in
`define_unknown_imports_as_traps` helper also had a functional async typing
mismatch; `facet_linker` supplies correctly typed sync and concurrent stubs.
