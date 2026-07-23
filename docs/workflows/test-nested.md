---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-07-03
---

# Test Nested Behavior

Use the built-in test CLI through the Nix development shell for automated behavior checks:

```sh
nix develop --command cargo run -- test
```

The explicit nested spelling is:

```sh
nix develop --command cargo run -- test nested
```

Run a real client inside the subject compositor:

```sh
nix develop --command cargo run -- test nested --inner-command 'weston-simple-egl -f'
```

Use a wrapper executable for anything that needs shell quoting, environment setup, private profiles, or log redirection. `--inner-command` is split into argv words before it reaches the DBus `spawn` path.

Keep the topology alive only when your verification script will clean it up:

```sh
nix develop --command cargo run -- test nested --keep-running --inner-command ./tmp-client-wrapper
```

The outer supervisor is offscreen by default. Pass `--outer-headful` only for host-visible manual inspection.

Nested debug dumps include `outer-dump/composed.png` and `outer-dump/state.json`. When the outer supervisor is used, hosted surfaces are rendered with test-only surface-local markers; `state.json` records each visible window's `marker_id` beside its expected rect.

Run the sandboxed software-rendered graphical proof through Nix:

```sh
nix build .#checks.$(nix eval --raw --impure --expr builtins.currentSystem).software-e2e
```

The result contains `inner-dump/` and `outer-dump/` with composed PNG captures and state manifests. The inner dump is the pixel proof for real clients in a split layout. The outer dump is a supervisor diagnostic for the hosted inner presentation.

Build the inspectable artifact bundle when the captured frame and decoded marker positions are the desired output:

```sh
nix build .#software-e2e-artifacts
```

The result contains `frame.png`, `layout-state.json`, and `detected-layout.json`.

## Cleanup TODO

TODO: make the checked-out source binary own the same `manifold test nested`
surface documented here, so repository proofs never depend on a separately
installed supervisor with a different parser. The supervisor should also own
shortcut isolation for a nested subject instead of requiring each workload to
toggle passthrough manually.

Fix nested debug-dump orientation as part of that cleanup. Captured surfaces
must have the same orientation as the real host-visible presentation, and the
software proof should assert a directional marker so a vertically inverted
capture cannot pass on color count alone. Once the consolidated proof is the
only supported entry point, remove the historical typed-key-policy wrapper.

Related:

- [[../systems/testing]]
- [[../systems/wayland-compositor]]
