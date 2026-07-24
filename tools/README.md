# Tools

Checked-in development and verification tools live here. Run them through the
repository's Nix environment.

## Primary Presentation Proofs

`test-client-layout-actor.sh` starts a real `manifold-client` under headless
Weston, maps real managed applications, and verifies client-owned layout,
managed-pane size convergence, and the presentation key-policy boundary.

`test-session-switcher-plugin-nested.sh` exercises the default client init
plugin and recursive layout chain. It drives real keys, clicks, and text;
switches, creates, renames, and closes sessions; hides and restores the
right-hand sidebar through an ordinary actor action; and retains graphical
proof artifacts.

Presentation proofs must use frontend-owned layout state and capture. Daemon
debug state remains useful for session and per-window transport diagnostics,
but the daemon is not a composed-frame or viewport-layout authority.

## CI And Host Graphical Checks

The software E2E proof runs the external WGPU client with real Wayland
workloads. It must run from a host environment with a DRM render node because
the daemon exports each managed window as a DMA-BUF:

```sh
nix run .#software-e2e-capture -- --out-dir /tmp/manifold-software-e2e
```

The output directory retains inspectable initial and reconnected
external-client frames plus backend inventory state. This proof is deliberately
not a pure Nix build check: a sandbox does not expose the host render node.

`audit-v1-capabilities.sh` and `smoke-v1-full.sh` collect broader compositor,
transport, input, status, and lifecycle checks. Presenter attach-failure tests
are intentionally absent: the backend has no native or offscreen presenter.

## Actor And Worker Checks

The `test-wasm-*`, runtime-I/O plugin, workspace-watch, and file-log workflows
exercise actor replacement, completion I/O, shared-memory workers, standard
WASI behavior, coordination, and session-scoped resources. Graphical variants
must start an external client rather than relying on daemon composition.

The deterministic Rube Goldberg quick profile is available as
`.#wasm-io-rube-quick-check`; stress and soak profiles are manual workloads and
should retain their reported seed.

## Headless Server Checks

Headless server checks cover nested Wayland globals, application mapping,
window lifecycle, session services, DBus, and transport protocol behavior.
They must not claim final layout, hit testing, or presentation pixels. Tests of
physical input, recursive layout, or frontend lifecycle belong in the external
client topology.
