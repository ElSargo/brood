# Terminal presentation actor

This first-party Component actor owns pane-local input and canvas rendering for
the terminal. The boot instance attaches to the session-owned
`terminal-session` actor; dynamically paired `terminal-ui-N` instances attach
to `terminal-session-N`. The actor
assembles revisioned snapshot chunks atomically, coalesces superseded revisions,
and renders styled runs through the public native monospace text service.

The actor keeps a guest-side layout cache keyed by visible styled run. Host text
resources which leave the accepted packet are dropped after replacement, making
layout churn and live-resource counts measurable without adding a terminal grid
to the host.

It encodes navigation and function keys with xterm modifiers and
application-cursor mode, routes committed text, and keeps scrollback navigation
local to the presentation/session actor protocol. Monospace layouts carry
normal/bold and normal/italic intent; other terminal decorations are canvas
commands.
