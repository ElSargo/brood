# Status bar presentation actor

The status bar is presentation-global shell chrome owned by the default client
init actor. It remains visible with an empty session catalogue and across
session changes. Its leading text reports the selected session and total
session count; the remaining space advertises the keyboard-shortcut catalogue,
session switcher, inbox, and terminal chords.

The session switcher owns placement and reserves the bottom strip in its root
layout. The status actor owns only its drawing and session-summary state. It
sends `status-bar-ready` with its pane identity after presentation creation and
ignores pane input.
