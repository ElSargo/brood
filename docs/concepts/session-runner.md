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
Numeric exit codes are preserved exactly. On Unix, signal termination is
reported to the invoking shell using the conventional `128 + signal` status.

Launch records are transport data, not display previews: project, working, and
cache paths and the program argument vector are serialized and restored without
truncation. Scope lookup parses the systemd scope as a complete path component,
so Unicode in surrounding cgroup components or a record identifier is never
sliced at a byte offset. If a UI later needs a bounded preview, that UI must
define its own display bound and truncate only at extended-grapheme boundaries;
the launch record remains the lossless source of truth.

`tools/test-unicode-launch-context.sh` exercises that contract through the real
systemd scope and `manifold-runner-exec` sidecar. Its Markdown artifact shows
the restored Unicode project, working, and cache paths plus a Unicode argument.
A plugin is not applicable at this boundary: launch-record recovery happens
before the entered command, session server, or plugin runtime starts.

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
