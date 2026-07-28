---
status: target
type: system
authority: product
last-reviewed: 2026-07-28
---

# First-Party Terminal Plugin

The Brood terminal is a bundled first-party Component plugin. It uses the same
public actor, PTY, input, text, canvas, and diagnostic interfaces available to
other installed plugins. First-party status controls packaging, defaults, and
support policy; it does not grant a private host terminal API.

## Composition

One session actor owns the shell child, PTY stream, terminal parser, main and
alternate screens, modes, bounded scrollback, and revision counter. One
presentation actor owns the pane, viewport position, key encoding, native text
layout cache, and canvas packets.

The boot pair uses `terminal-session` and `terminal-ui`. `Ctrl+Alt+T` sends an
ordinary product action to the client init actor. It asks the active session's
init actor to start a capability-scoped `terminal-session-N`, then starts the
matching `terminal-ui-N` only after the session actor reports ready. The new UI
uses the general deferred pane-focus command while recursive layout catches up;
the key policy does not spawn the process itself.

PTY output is parsed in a session worker. The first output after terminal input
is published without a batching delay; autonomous and sustained output drains a
short bounded burst before signaling a changed generation. The actor publishes
a chunked complete bootstrap followed by changed-row revisions rather than
converting each native read into a mailbox message or repeatedly transporting
an unchanged grid. The presentation actor assembles one revision atomically,
applies deltas only to a compatible complete state, and supersedes stale render
work.

The PTY shell receives the same host-controlled nested display, surface-session
identity, control identity, and isolated session bus as other managed backend
children. GUI descendants therefore map into the owning Brood session.
Single-instance clients such as Ghostty cannot escape through an inherited host
session bus and create a normal desktop window.

The snapshot is an application protocol over ordinary bounded actor messages.
It includes the visible rows, cursor and input modes, viewport extent, child
state, and pressure counters. It is not a host screen-grid protocol.

## Terminal Core

The guest terminal core uses a real VT parser library and owns:

- printable Unicode cells, combining characters, and double-width cells;
- main and alternate screens, cursor state, scrolling regions, and bounded
  scrollback;
- indexed and truecolor SGR plus weight, italic, faint, inverse, underline, and
  strike styles;
- erase, insert, delete, scroll, cursor, origin, wrap, and application-cursor
  modes needed by interactive shells and full-screen applications;
- device responses emitted back through the same bounded PTY stream.

Resizing updates both saved and active guest buffers before resizing the native
PTY. Repeated resize in an alternate-screen application must therefore leave a
valid main screen when the application exits. Reflow, selection, hyperlinks,
mouse reporting, and additional OSC/DCS behavior remain terminal-library and
plugin work; they must not move into the host.

## Input And Rendering

The presentation actor converts stable semantic key events into xterm input.
Navigation and function keys honor Ctrl, Alt, and Shift modifier parameters;
cursor keys follow the application-cursor mode published by the session actor.
Committed text uses the text event path, so printable input is not inferred
from a physical keyboard layout.

Shift+PageUp, Shift+PageDown, Shift+Home, Shift+End, and pointer scroll move the
guest viewport without writing to the PTY. Any ordinary terminal input returns
to the live bottom. The session actor retains scrollback and supplies only the
requested visible rows.

Rendering groups adjacent cells into guest-owned styled runs. Static rows use
run layouts, while the active cursor row uses reusable glyph-cluster layouts so
each echoed character does not synchronously relayout the growing line. The
actor requests monospace native layouts with stable cell metrics, caches a
bounded recent set, draws backgrounds and decorations with normal canvas
commands, and evicts least-recently-used inactive handles after replacing the
packet. The client sees immutable text resources and drawing commands, never
terminal cells. Snapshot byte, changed-row, full-bootstrap, packet,
cache-reuse, and eviction counts expose transport volume, coalescing, and churn
directly. Host presentation diagnostics report every covered terminal input,
attributed by backend, with tail percentiles and standard deviation rather than
only an easily misinterpreted maximum.

## Lifecycle

Presentation disconnect destroys pane-local workers, packets, and text
resources. It does not stop the session actor or its shell. A replacement
presentation actor attaches to the existing session actor and receives a
complete current revision.

The current reconnect bootstrap reconstructs the boot terminal pair only.
Shortcut-created session actors still survive a presentation disconnect, but
remain headless after reconnect. Replaying a session-init terminal inventory to
the replacement client init actor is the remaining lifecycle step; process
ownership must not be moved into the client to avoid it.

The PTY remains actor-generation-owned today. Replacing the session actor
therefore tears down its child and streams. Surviving session-actor replacement
requires the explicitly supervised session-resource promotion described in
[[plugin-io-and-resources]], not detached processes or host-owned emulation.

## Product Gaps And Required General Primitives

The terminal workload exposes these remaining general host gaps:

- Clipboard access needs a focus- and lease-checked text clipboard capability.
  Selection and copy/paste stay in the guest; the host only bridges the
  platform clipboard and session boundary.
- Complete IME support needs pane-local preedit, commit, cancellation, and
  candidate-rectangle events. The terminal presentation actor should render
  preedit state and send only committed text to the PTY.
- Shell choice and terminal preferences need instance configuration delivered
  to the actors. The selected executable must still have an explicit
  capability allowlist entry. Ambient process execution is not a substitute.
- Optional OSC title and bell integration needs constrained pane metadata and
  attention requests. It does not justify exposing native window objects.

The checked-in terminal currently receives committed text but not an IME
preedit lifecycle, has no clipboard bridge, and launches the configured
allowlisted `sh`. Those limits materially affect daily terminal use and are the
next reusable interfaces to close.

## Acceptance

The built-in Nix-backed nested proof drives a real presentation client and PTY.
It verifies shell interaction, ANSI styles and Unicode, exact function and
modified-key bytes, application-cursor negotiation, Helix, alternate-screen
resize, scrollback navigation, presentation replacement, a real managed Ghostty
descendant, sustained output, real `Ctrl+Alt+T` creation of a fresh paired
terminal, child exit, and zero leaked runtime or text resources. It captures
screenshots and structured latency, mailbox, stream, packet, and text-churn
metrics.

Related:

- [[plugin-runtime]]
- [[plugin-io-and-resources]]
- [[plugin-ui]]
- [[plugin-graphics]]
- [[presentation-client]]
- [[../workflows/test-nested]]
