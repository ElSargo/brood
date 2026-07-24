---
status: canonical
type: workflow
authority: product
last-reviewed: 2026-07-24
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

Run the packaged end-to-end capture on a host with a DRM render node:

```sh
nix run .#software-e2e-capture -- --out-dir /tmp/manifold-software-e2e
```

The output directory contains the initial and reconnected client-composed
frames. A backend state dump accompanies the proof for pane identity and
frame-production evidence, but it is not the layout or pixel authority. This
is a host-runtime proof rather than a pure Nix build check because DMA-BUF
export requires access to the render node.

Related:

- [[../systems/testing]]
- [[../systems/wayland-compositor]]
