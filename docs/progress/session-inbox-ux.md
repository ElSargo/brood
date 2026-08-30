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

## Transition race follow-up

The retained nested artifact
`/tmp/brood-integration-session-picker-7c63b5d` exposed a real input race. Its
client log records synchronized request 6 reserving target revision 8, then a
remapped keyboard action reaching the switcher before request 6 completed.
The switcher submitted request 7 against its still-observed revision 7, and
the host rejected it as `StaleBaseLayoutRevision`; the first user toggle was
therefore lost. Ordinary sidebar and inbox transitions now use the existing
latest-state deferral discipline while any shell transaction is in flight and
republish after its completion notification supplies the reserved target
revision. Pointer interaction chains and native resize retries retain their
separate transaction paths.

Repeated backend-picker requests now cancel an already-created picker before
opening its replacement. Picker selections are also admitted only when their
picker ID matches the current backend-picker intent, so a late selection from
the cancelled surface cannot create or activate a session. Recently cancelled
and completed backend-picker identities remain in a bounded retirement queue,
covering a stale selection that arrives after the replacement itself has
already completed and cleared its request token.

Deferred shell intent is consumed on every terminal transaction path, not
only completion and degraded recovery. When a request is rejected, aborted,
or superseded and no other shell request remains, the newest deferred intent
is immediately republished against the usable base revision. Stale-inventory
recovery instead folds that intent into the state awaiting the host's fresh
layout publication. This prevents an older deferred intent from surviving a
rejection and replaying after an unrelated newer transition.

The focused Nix-backed session-switcher suite now passes 35 tests, including
regressions for a keyboard toggle during a synchronized transition and for a
selection from a replaced backend picker. All-target Clippy passes with
warnings denied. The retained graphical run was inspected rather than rerun
to avoid contending with the active integrated graphical verification lane.
