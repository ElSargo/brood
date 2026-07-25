---
status: current
type: reference
authority: implementation
last-reviewed: 2026-07-03
code-links:
  - crates/broodd/src/lib.rs
  - crates/broodd/src/control.rs
  - crates/brood-remote/src/main.rs
  - crates/brood-remote/src/ssh.rs
---

# CLI Reference Pointers

Exact top-level CLI flags, defaults, aliases, and hidden test options are defined in `crates/broodd/src/lib.rs`. `ctl` subcommands and typed DBus helper commands are defined in `crates/broodd/src/control.rs`.

Current top-level commands:

- `check`
- `enter`
- `show`
- `hide`
- `list`
- `run`
- `plug`
- `switch`
- `test`
- `ctl`

Use this page as an orientation map only. Do not duplicate full parser behavior in prose.

Remote relay, probe, certificate, and one-command SSH deployment behavior is
defined by the `brood-remote` parser. See [[../workflows/ssh-deploy]] for the
operator workflow.

Related:

- [[../workflows/run-brood]]
- [[../workflows/enter-session]]
- [[../workflows/test-nested]]
