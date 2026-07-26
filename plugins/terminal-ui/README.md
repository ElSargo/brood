# Terminal presentation actor

This first-party Component actor owns pane-local input and canvas rendering for
the terminal. The boot instance attaches to the session-owned
`terminal-session` actor; dynamically paired `terminal-ui-N` instances attach
to `terminal-session-N`. The actor
assembles revisioned snapshot chunks atomically, coalesces superseded revisions,
and renders styled runs through the public native monospace text service.

The actor keeps a 512-entry guest-side recent-layout cache keyed by styled run.
Inactive least-recently-used handles are dropped only when that bound is
exceeded, preserving TUI working sets while keeping churn and live-resource
counts measurable without adding a terminal grid to the host.

It encodes control, navigation, and function keys with xterm modifiers and
application-cursor mode. Smooth wheel and trackpad deltas accumulate into
guest-owned scrollback movement; applications that negotiate xterm mouse
tracking instead receive SGR mouse reports. Holding Shift keeps pointer input
local so text can still be selected inside mouse-aware applications.

Drag selection, selection highlighting, copied-text extraction, and bracketed
paste remain presentation-guest behavior. The client supplies only a
presentation-local clipboard bridge. Selection copies on release, while
`Ctrl-Shift-C`, `Ctrl-Shift-V`, `Shift-Insert`, and middle click provide the
usual explicit copy/paste paths. Monospace layouts carry normal/bold and
normal/italic intent; other terminal decorations are canvas commands.
