---
status: friction
type: design-input
authority: implementation
---

# Terminal Plugin Frictions

## Reattachment ownership

The PTY is owned by its session actor generation. Replacing that generation
tears down the shell and streams. Presentation replacement now replays the
complete ready terminal inventory through the shared SDK helper; see
[[../progress/terminal-reattachment]].

The target requires reattachment without moving terminal emulation or process
ownership into the presentation client. The open design questions are how
session-resource promotion supervises the PTY across actor replacement.

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
