---
status: friction
type: design-input
authority: implementation
---

# Terminal Plugin Frictions

## Reattachment ownership

The PTY is owned by its session actor generation. Replacing that generation
tears down the shell and streams. Shortcut-created terminal actors also survive
a presentation disconnect without being replayed to replacement client init,
which leaves them headless.

The target requires reattachment without moving terminal emulation or process
ownership into the presentation client. The open design questions are how
session-resource promotion supervises the PTY across actor replacement and how
the session terminal inventory is replayed to a replacement presentation.

## Clipboard and IME

The current terminal receives committed text but has no clipboard bridge or IME
preedit lifecycle. The target needs a focus- and lease-checked text clipboard
capability plus pane-local preedit, commit, cancellation, and candidate-rectangle
events without exposing platform window objects.

## Configuration and constrained metadata

Shell choice and terminal preferences need instance configuration delivered to
the actors while executable selection remains allowlisted. Optional OSC title
and bell integration needs constrained pane metadata and attention requests.
The design must provide these as reusable plugin facilities rather than
terminal-specific host behavior.
