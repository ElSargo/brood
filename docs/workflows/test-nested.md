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

The Chromium launch proof exercises normal and minimum-constrained pane sizes
with real page rendering, keyboard input, clicks, and scrolling:

```sh
nix develop --command cargo run -- test chromium --out-dir /tmp/brood-chromium-proof
```

It requires Chromium on the test environment's path. Workload setup and binary
overrides are defined in `tools/test-chromium-nested.sh` and
`tools/test-chromium-nested.py`.

Audit or run the complete bundled-plugin matrix with:

```sh
nix develop --command ./tools/test-bundled-plugins.sh --audit
nix develop --command ./tools/test-bundled-plugins.sh --all --out-dir /tmp/brood-plugin-e2e
```

Use `--list` to inspect the external assertion owned by each plugin and
`--lane NAME` to run one focused workload. The parser in
`tools/test-bundled-plugins.sh` defines the exact accepted options. Every lane
retains its own log and evidence directory below the suite output.

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
- [[../targets/systems/terminal-plugin]]
