---
status: current
type: concept
authority: implementation
last-reviewed: 2026-07-01
code-links:
  - crates/manifoldd/src/session_runner.rs
  - crates/manifoldd/src/bin/manifold-runner-nix-develop.rs
---

# Session Runner

A session runner activates a project environment before Manifold starts a session server or entered command.

Current runner ABI:

```text
runner <project-dir> <working-dir> <session-cache-dir> <runner-cache-root> <inner-binary>
```

The runner activates its environment, exports whatever variables the project needs, runs the supplied inner binary, and exits with that binary's status.

Current built-in runner:

- `nix-develop`

Runner discovery currently walks upward from the command cwd:

- an enabled `[session.runner]` in `manifold.toml` wins
- `enabled = false` disables runner discovery for that project
- otherwise `flake.nix` synthesizes a `nix-develop` runner

Related:

- [[../systems/runtime]]
- [[../systems/configuration]]
- [[../workflows/enter-session]]
