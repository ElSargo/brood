---
status: canonical
type: workflow
authority: product
last-reviewed: 2026-07-25
---

# Test Nested Behavior

Run checked-in behavior proofs through the Nix development environment:

```sh
nix develop --command ./tools/test-client-layout-actor.sh
nix develop --command ./tools/test-session-switcher-plugin-nested.sh
```

These workflows launch a real external presentation client under an isolated
graphical compositor. They drive input through that frontend and retain
frontend-owned logs and captures.

The bundled terminal proof is exposed through the built-in test topology:

```sh
nix develop --command cargo run -- test nested --out-dir /tmp/brood-terminal-proof
```

It builds the current source through the flake, starts the session service with
offscreen presentation, then drives a real external client under isolated
headless Sway. The output directory contains structured runtime, mailbox,
graphics, text-resource, and key-to-present metrics plus captures for shell
ANSI/Unicode/style output, exact modified and application-cursor key bytes,
presentation reconnect, Helix, repeated alternate-screen resize, guest
scrollback, streaming output, and child teardown. Exact command parsing remains defined in
`crates/brood-cli/src/cli.rs`.

Run the packaged end-to-end capture on a host with a DRM render node:

```sh
nix run .#software-e2e-capture -- --out-dir /tmp/brood-software-e2e
```

The output directory contains the initial and reconnected client-composed
frames. A backend state dump accompanies the proof for pane identity and
frame-production evidence, but it is not the layout or pixel authority. This
is a host-runtime proof rather than a pure Nix build check because DMA-BUF
export requires access to the render node.

Related:

- [[../systems/testing]]
- [[../systems/wayland-compositor]]
- [[../systems/terminal-plugin]]
