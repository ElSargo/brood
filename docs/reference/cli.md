---
status: current
type: reference
authority: implementation
last-reviewed: 2026-07-03
code-links:
  - crates/broodd/src/lib.rs
  - crates/broodd/src/control.rs
  - crates/bmux/src/main.rs
  - crates/brood-remote/src/main.rs
  - crates/brood-remote/src/ssh.rs
  - crates/brood-remote/src/container.rs
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

The separately installed `bmux` executable provides the tmux-shaped interface
to Brood sessions and first-party terminals. Its exact command aliases, target
grammar, formats, and key encodings are defined in `crates/bmux/src/main.rs`;
the product mapping and compatibility boundary are described in
[[../targets/systems/bmux]].

Remote relay, probe, certificate, and one-command SSH deployment behavior is
defined by the `brood-remote` parser. See [[../workflows/ssh-deploy]] for the
SSH operator workflow and [[../workflows/container-backends]] for local and
remote Podman-backed sessions.

Related:

- [[../workflows/run-brood]]
- [[../workflows/enter-session]]
- [[../workflows/test-nested]]
