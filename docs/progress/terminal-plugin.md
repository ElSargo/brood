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
