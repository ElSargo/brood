# Terminal session actor

This first-party Component actor owns one allowlisted shell, its PTY, terminal
parser, live grid, and bounded scrollback. It publishes revisioned, row-chunked
snapshots to an attached `terminal-ui` presentation actor. PTY output wakes the
actor through a coalescing notification, so output chunks never become a
mailbox-message stream.

The guest core implements main/alternate buffers, scroll regions, editing and
cursor modes, application-cursor and xterm mouse-mode negotiation, bracketed
paste, Unicode cell widths, ANSI and truecolor styles, and a
presentation-requested scrollback viewport. Repeated alternate-screen resize
updates both buffers before resizing the PTY.

The actor requires `pty` and `subprocess:sh`. The host supplies the session
working directory and the deterministic `xterm-256color`/truecolor terminal
profile.
