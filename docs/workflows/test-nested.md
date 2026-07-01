---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-07-01
---

# Test Nested Behavior

Use the built-in test CLI for automated behavior checks:

```sh
cargo run -- test
```

The explicit nested spelling is:

```sh
cargo run -- test nested
```

Run a real client inside the subject compositor:

```sh
cargo run -- test nested --inner-command 'weston-simple-egl -f'
```

Use a wrapper executable for anything that needs shell quoting, environment setup, private profiles, or log redirection. `--inner-command` is split into argv words before it reaches the DBus `spawn` path.

Keep the topology alive only when your verification script will clean it up:

```sh
cargo run -- test nested --keep-running --inner-command ./tmp-client-wrapper
```

The outer supervisor is offscreen by default. Pass `--outer-headful` only for host-visible manual inspection.

Related:

- [[../systems/testing]]
- [[../systems/wayland-compositor]]
