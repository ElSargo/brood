---
status: progress
type: implementation-log
authority: implementation
last-reviewed: 2026-08-31
---

# Session And Inbox UX Progress

The presentation-global inbox now has one reversible visibility interaction:
the existing shortcut toggles the rail, while Escape and a pointer close
control hide it. Keyboard selection, activation, and dismissal share the same
bounded queue mutations as pointer activation. Selection and scroll are
clamped after queue growth, removal, and resize.

The root shell no longer records its own inbox pane as the last workspace
focus. This prevents a later hide transaction from restoring focus to the
hidden rail. The switcher also gives repeated asynchronous backend-picker
requests unique intent correlations. A late creation reply from a superseded
request is cancelled instead of acquiring authority over the current loading
state.

Focused unit tests cover inbox authentication and bounds, keyboard activation,
toggle messaging, shell-focus exclusion, distinct repeated picker intents, and
stale picker-reply admission. Nix-backed verification passed on 2026-08-31:

- `cargo test --manifest-path plugins/inbox/Cargo.toml`: 6 passed;
- `cargo test --manifest-path plugins/session-switcher/Cargo.toml`: 34 passed;
- focused all-target Clippy with warnings denied for both packages;
- release `wasm32-unknown-unknown` builds for both components.

No nested presentation run was added to this focused pass. Existing nested
workloads cover hidden inbox composition, shortcut focus, notification click
activation, and session-switcher pointer states, but do not yet exercise the
new second shortcut press or inbox keyboard dismissal as acceptance steps.
