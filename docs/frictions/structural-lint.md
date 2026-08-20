---
status: friction
type: design-input
authority: implementation
---

# Structural Lint Friction

## Daemon actor addressing

The session picker currently reaches the daemon-scoped worktree manager through
the DBus control endpoint. This is an accepted temporary bridge while actor
messaging addresses only actors in the presentation's bound surface session.

The mismatch keeps an internal product interaction on an external control
adapter. Once daemon-realm actor addressing and correlated replies are
available, picker requests and completions should use ordinary actor messages.
DBus should remain an external control adapter over that actor protocol.

The open design question is how daemon-realm addresses, authorization, and
correlated replies fit the target actor namespace without coupling a picker to
one transport.

## CLI command normalization

CLI command/default resolution still uses local option branching in
`crates/broodd/src/lib.rs`. The crate carries a scoped lint allowance and a
TODO because removing the warning locally would preserve the underlying ad hoc
boundary.

The open design question is which typed helper owns command and default
resolution without moving process-exit policy below the binary entrypoint.

## DBus control dispatch

DBus proxy waits still cross the compositor actor boundary in
`crates/broodd/src/control.rs`. That module carries scoped lint allowances while
the external proxy ABI and compositor-state dispatch remain combined.

The open design question is how to separate the proxy ABI from actor dispatch
while retaining typed failures, bounded waits, and the existing external
control contract.
