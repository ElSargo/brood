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

Nested debug dumps include `outer-dump/composed.png` and `outer-dump/state.json`. When the outer supervisor is used, hosted surfaces are rendered with test-only surface-local markers; `state.json` records each visible window's `marker_id` beside its expected rect.

Run the sandboxed software-rendered graphical proof through Nix:

```sh
nix build .#checks.$(nix eval --raw --impure --expr builtins.currentSystem).software-e2e
```

The result contains `inner-dump/` and `outer-dump/` with composed PNG captures and state manifests. The inner dump exercises real clients in a split layout; the outer dump proves the same session can be presented and captured through the offscreen supervisor.

Related:

- [[../systems/testing]]
- [[../systems/wayland-compositor]]
