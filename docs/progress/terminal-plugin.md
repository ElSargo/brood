---
status: progress
type: verification-record
authority: implementation
---

# Terminal Plugin Progress

The checked-in terminal receives committed text, launches the configured
allowlisted `sh`, and keeps the boot terminal pair available across presentation
replacement. Shortcut-created terminal actors survive presentation disconnect,
but their inventory is not replayed to a replacement client and they remain
headless.

The Nix-backed nested proof drives a real presentation client and PTY. It covers
shell interaction, ANSI styles and Unicode, function and modified-key bytes,
application-cursor negotiation, Helix, alternate-screen resize, scrollback,
presentation replacement, a managed Ghostty descendant, sustained output,
`Ctrl+Alt+T` terminal creation, child exit, and resource cleanup. Screenshots
and structured latency, mailbox, stream, packet, and text-churn metrics provide
the observable evidence.

The target product boundary is [[../targets/systems/terminal-plugin]]. Remaining
design-level gaps are in [[../frictions/terminal-plugin]].

## 2026-08-31 interaction polish

The presentation actor now paints a selection across every selected cell,
including spaces and gaps between styled runs, rather than limiting the visible
highlight to cells which happen to carry a text run. Its cursor uses the shared
Ashen accent and blinks on a single-flight 530 ms worker while the pane is
focused. Input immediately restores the visible phase; losing focus leaves a
steady cursor and stops scheduling blink workers.

The Nix-backed focused terminal UI suite passed 10 tests, and the crate passed
Clippy with warnings denied. The packaged nested terminal proof remains the
graphical acceptance gate for the interaction as a whole.
