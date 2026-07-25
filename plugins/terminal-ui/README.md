# Terminal presentation actor

This first-party Component actor owns pane-local input and canvas rendering for
the terminal. It attaches to the session-owned `terminal-session` actor,
assembles revisioned snapshot chunks atomically, coalesces superseded revisions,
and renders styled runs through the public native monospace text service.

The actor keeps a guest-side layout cache keyed by visible styled run. Host text
resources which leave the accepted packet are dropped after replacement, making
layout churn and live-resource counts measurable without adding a terminal grid
to the host.
