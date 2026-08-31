---
status: target
type: system
authority: product
last-reviewed: 2026-08-31
---

# bmux

`bmux` is the tmux-shaped command interface to Brood. A bmux session is a
Brood surface session and a bmux pane is a first-party Brood terminal actor.
The Brood server remains authoritative for names, lifecycle, working
directories, active membership, terminal processes, and presentation; bmux
must not maintain a parallel session database.

## Compatibility Boundary

The important tmux automation vocabulary is supported with tmux command names,
aliases, targets, exit behavior, and commonly consumed formats. It includes
session creation, attachment, discovery, switching, renaming, and destruction;
pane creation, discovery, selection, input, and destruction; isolated named
servers; command sequences; detached creation; and machine-readable format
strings.

Brood has one workspace per session rather than tmux's extra window layer.
Window commands therefore address a session's single workspace window, while
both `new-window` and `split-window` create a real Brood terminal pane. This is
an intentional structural mapping, not hidden bmux-only state.

Attachment presents or focuses the graphical Brood client and returns after
the request succeeds. It does not turn the invoking terminal into a character
cell client. Detachment closes that presentation without terminating the
Brood sessions or terminal actors.

tmux facilities whose contract requires a server-owned character-cell grid,
copy-mode buffer, command prompt, status-line option store, or tmux control
protocol are outside this interface. Terminal screen state remains inside the
terminal plugin as required by [[terminal-plugin]]. Brood keybindings, layout,
and UI plugins remain the authority for those concerns.

## Control Path

bmux uses the same private local control transport and typed session API as the
Brood CLI. Terminal lifecycle requests go through the session init actor, so a
new pane is the standard paired terminal session/presentation actor. Input is
delivered to that terminal actor through its normal sequenced PTY mailbox
protocol. Focus and teardown go through the blessed presentation init actor so
the visible pane and backend actor lifecycle remain paired.

Targets expose stable tmux-style session and pane identifiers derived from
Brood session ids and terminal ordinals. Named bmux servers map to isolated
Brood control destinations.

## Acceptance

Acceptance exercises the binary against a real Nix-backed nested Brood server.
It creates detached named sessions, starts commands in first-party terminals,
adds panes, sends literal and named keys, selects and removes panes, attaches
and detaches presentation, checks formats and targets, and tears sessions down
without orphaned terminal actors.

Implementation evidence is in [[../../progress/bmux]].
