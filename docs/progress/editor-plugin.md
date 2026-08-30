---
status: progress
type: verification-record
authority: implementation
---

# Editor Plugin Progress

## 2026-08-31 startup and malformed-operation hardening

Editor startup mirror discovery no longer resolves and stats every ordinary
configuration and data file after already inspecting it. Ordinary entries use
one metadata lookup; symlinks are followed explicitly, and directory cycles are
detected by filesystem identity. This removes two synchronous filesystem probes
per ordinary mirrored file from the local editor startup path without changing
the read-only snapshot boundary.

An invalid or out-of-order streamed-open chunk now releases its assembly slot.
Previously, four malformed continuations could retain all four bounded transfer
slots and prevent later large-file opens until the presentation-local Neovim
process restarted. The Wasm runner smoke workload now submits eight malformed
transfers, requires each structured rejection, and then requires a healthy open
from the same process. It also records time from runner spawn to the ready
message so startup regressions have a directly comparable measurement.

The focused Neovim client suite passed 15 tests and Clippy with warnings denied.
The runner and smoke scripts pass Node syntax checks. The real packaged Wasm
runtime smoke passed with `startup_ms=9675`, eight malformed-transfer
recoveries, remote open/save lifecycle coverage, Tree-sitter parsing,
backpressure, escape-heavy screen state, and a clean shutdown. The nested
editor proof remains the full graphical acceptance gate.
