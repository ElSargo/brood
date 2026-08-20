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

- a fresh catalogue remains empty, with no terminal actor or pane;
- the switcher and 34-pixel status bar are the only composed startup panes,
  while the inbox actor is live but hidden;
- real `Ctrl+Shift+/` input opens the shortcut catalogue from that empty state;
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

## Synchronized Resize Proof

The built-in resize proof runs through the checked-in test CLI in the Nix
environment. It starts a real presentation client under the private compositor
and retains parent, window-shell, recursive-region, and managed-client proof
artifacts. The synchronized protocol deadline remains 250 ms. Screenshot
completion is not part of transaction control.

The managed lane uses real XDG shared-memory clients at fractional scale 1.5.
It correlates protocol version 22 records across client and server diagnostics:
accepted epoch and route generation, configure serial, configured size tuple,
new commit samples, exact snapshot, scene sequence and transport resource,
client stage, readiness, and atomic promotion. Normal pointer and native resize
must combine plugin and managed waits without a partial waited-set frame.

The proof also supplies an acknowledged configure with an old or insufficient
buffer, supersedes one epoch and delivers its late tag, withholds one managed
pane through timeout, and removes a named pane during an accepted transaction.
It must show:

- unsuitable buffers cannot satisfy readiness;
- a stale tagged resource cannot replace the active epoch;
- timeout occurs from 250 ms through less than 750 ms and presents one complete
  old or placeholder managed set before ordinary recovery;
- staged resources and expectations remain bounded and clear at terminal
  transitions;
- ordinary recovery starts only after a later untagged commit;
- removal aborts with the exact unavailable pane, never promotes that target,
  and publishes later inventory without the pane;
- the final pointer release remains authenticated after capture ends and
  converges normally;
- pane content and one-device-pixel native chrome align in the same presented
  geometry.

Stored analyzers join evidence by epoch, backend route, connection generation,
pane, configure serial, surface commit, scene sequence, resource, and exact size
tuple. They do not infer success from response order or from the presence of a
screenshot alone.

`tools/test-bundled-plugins.sh` audits the bundled plugin inventory against
`tools/bundled-plugin-e2e.tsv` and routes each plugin to a Nix-backed real
workload lane. A row is complete only when synthetic input reaches the plugin
and an external boundary records the result: composed pixels, persisted bytes,
process or actor state, or an independently observed protocol exchange.

## Lifecycle Proof

A frontend replacement proof closes or kills only `brood-client`, confirms
that the session server and hosted applications remain alive, then starts a
replacement client. The replacement receives the existing pane identities and
frames, obtains new presentation and layout-lease generations, and reconstructs
the sidebar/workspace composition.

## Bundled Plugin Acceptance

`tools/test-bundled-plugins.sh` is the complete first-party plugin acceptance
index. Its checked-in matrix must name every crate below `plugins/`; inventory
drift is a test failure rather than an undocumented coverage gap. Each matrix
row identifies a real synthetic-input lane and the external evidence that lane
must retain.

The empty-startup lane connects a real presentation client before any surface
session exists. It requires a composed session picker and status bar, keeps the
global inbox out of the visible layout until requested, and opens the hotkey
list with a physical frontend chord. The general session-shell lane then uses
populated sessions for switching, picker, diff, worktree, and pointer behavior.

Non-visual runtime plugins use workspace files as their product boundary. The
runtime lane sends actor messages, creates native file events, exercises PTY,
pipe, timer, and loopback readiness, hot-replaces a Component watch generation,
and retains guest-written transcripts plus structured host resource state.

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
- [[../targets/systems/presentation-client]]
- [[../targets/systems/layout-actor]]
- [[wayland-compositor]]
