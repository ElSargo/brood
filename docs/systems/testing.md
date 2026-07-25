---
status: canonical
type: system
authority: product
last-reviewed: 2026-07-24
code-links:
  - clients/brood-client/src/main.rs
  - crates/brood-layout/src/lib.rs
  - nix/packages.nix
  - tools/test-client-layout-actor.sh
  - tools/test-session-switcher-plugin-nested.sh
---

# Testing

Behavior tests use the checked-in Nix environment and exercise the external
`brood-client`. Product runtime commands are for normal or manual use, not
as an automated substitute for the test supervisor.

## Graphical Topology

An automated graphical proof starts a session server and a real presentation
client under a private compositor such as headless Weston. Physical keys,
pointer motion, clicks, and scroll enter through the frontend. The frontend
owns layout inspection and composed capture; the server may separately record
session state and raw per-window export diagnostics.

The proof must not:

- start a backend Winit or offscreen presenter;
- use daemon layout commands or resolved-layout status;
- inject global viewport coordinates into the daemon;
- use a daemon-composed screenshot as presentation evidence.

## Recursive Layout Proof

`tools/test-client-layout-actor.sh` is the focused layout boundary proof. It
starts the real client, loads the workspace actor, maps real managed clients,
and verifies that accepted client-owned layout sizes converge through the
server's pane-size protocol.

`tools/test-session-switcher-plugin-nested.sh` is the full wrapper proof. It
must demonstrate:

- the switcher claims the root lease;
- its `rest` slot is delegated to the workspace actor;
- the visible switcher occupies a right sidebar;
- the key policy sends an ordinary action to the switcher actor;
- hiding republishes the root fragment with a full-viewport `rest`;
- showing restores the same plugin pane and focuses it;
- `Ctrl+Alt+P` opens the reusable picker above the retained recursive
  composition, real fuzzy text selects a result, and completion removes the
  floating pane without a native-layout fallback;
- the plugin canvas target and resize event converge to the sidebar's own
  extent, rather than the full presentation extent;
- no switcher-specific host visibility mutation or backend placement occurs.

Screenshots should cover visible, picker-overlay, hidden, and restored states.
Logs should record lease generations, revisions, accepted recursive
composition, picker focus and sizing, and managed-pane size convergence.

## Lifecycle Proof

A frontend replacement proof closes or kills only `brood-client`, confirms
that the session server and hosted applications remain alive, then starts a
replacement client. The replacement receives the existing pane identities and
frames, obtains new presentation and layout-lease generations, and reconstructs
the sidebar/workspace composition.

## Acceptance And CI

The repository does not maintain implementation-detail tests as product
acceptance. A retained behavior test must exercise current, externally visible
behavior through a real workload and leave evidence at the boundary whose
behavior is being fixed.

`software-e2e-capture.sh` uses Mesa software rendering and a real external
client. Its initial and reconnected frames are frontend artifacts. The workload
must fail if it silently falls back to a backend presenter.

CI owns reproducible build and lint correctness. Graphical acceptance remains a
host runtime workload because DMA-BUF export requires a render node that is not
available in a pure Nix build sandbox.

## Related Pages

- [[../workflows/test-nested]]
- [[presentation-client]]
- [[layout-actor]]
- [[wayland-compositor]]
