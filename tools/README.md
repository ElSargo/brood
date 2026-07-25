# Tools

Checked-in development and verification tools live here. Run them through the
repository's Nix environment.

## Presentation Proofs

`test-client-layout-actor.sh` starts a real `brood-client` under headless
Weston, maps real managed applications, and verifies client-owned layout,
managed-pane size convergence, and the presentation key-policy boundary.

`test-session-switcher-plugin-nested.sh` exercises the default client init
plugin and recursive layout chain. It starts real windows in two sessions
before the client connects, verifies that both enter the client catalog and
layout lease while only the selected session is shown by default, and captures
the visible result on switch. Catalog revisions must arrive through the
dedicated shared-memory lane rather than a DBus polling worker. It also drives
real keys, clicks, and text;
creates, renames, and closes sessions; hides and restores the right-hand
sidebar through an ordinary actor action; and retains graphical proof
artifacts.

`software-e2e-capture.sh` runs the external WGPU client with real Wayland
workloads and retains initial and reconnected client frames:

```sh
nix run .#software-e2e-capture -- --out-dir /tmp/brood-software-e2e
```

These are the retained acceptance paths because they exercise current,
externally visible behavior through a real presentation client. Presentation
proofs use frontend-owned layout state, real input, and client capture. Server
diagnostics may accompany a proof, but are not acceptance evidence for final
layout, hit testing, or pixels.
