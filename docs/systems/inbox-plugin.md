---
status: target
type: system
authority: product
last-reviewed: 2026-07-30
---

# Inbox Plugin

The inbox is a presentation-global notification queue. One inbox actor belongs
to one connected client, remains visible while that client moves among backend
sessions, and never becomes session-owned application state.

## Ownership And Placement

The client init actor owns one `inbox` child without a backend or session
association. Its pane starts hidden so a fresh presentation remains an empty
workspace beside the session picker. The root layout reveals a rail when the
user asks for the inbox and delegates ordinary backend and plugin panes through
the remaining layout slot. Changing the active session does not replace,
clear, or rebind the inbox.

The queue is bounded presentation state. Reconnecting a client creates a fresh
queue; backend agents, terminals, processes, and their histories retain their
existing session lifetimes.

## Notification Contract

A session actor sends a bounded `inbox-push` application message containing:

- a short category, title, detail, and completion status;
- the presentation actor that represents the work which completed.

The target is an actor address rather than a pane number. Backend pane IDs are
not presentation-global, while the target actor already owns the correct pane
identity and any attach or resize work required when it regains focus.

The inbox accepts pushes only from authenticated session routes. Clicking a row
removes it from the queue and sends the target actor a `focus` message. Stale
rows whose source actor has closed are harmless and can still be dismissed.
The newest item appears first, and pointer scrolling exposes older queued
items without changing application focus.

`Ctrl+Alt+I` sends the inbox a client-local `show` action. The inbox asks the
root shell to reveal and focus its pane; queued notifications remain available
while the rail is hidden.

## Producers

Coding-agent actors publish when a turn finishes or fails. The notification
targets the UI that submitted the turn, or the first subscribed UI that appears
while a non-visual caller starts the turn.

Terminal session actors publish after an interactive shell command returns.
The shell prompt emits a private terminal marker containing the exit status;
the terminal guest consumes that marker without rendering it and accepts it
only after the UI submitted a line. This detects completion after full-screen
programs and long-running commands without guessing from visible prompt text,
while prompt redraws caused by startup or resize stay silent.

Other actors may use the same message contract when they have a meaningful
completion boundary and a presentation actor to activate.

## Verification

Acceptance uses the nested client path. It must show the inbox alongside the
ordinary session UI, run real terminal and agent work, observe queue rows in a
screenshot, click a row with pointer input, and verify focus reaches the source
pane. Tests also cover bounded queue behavior, malformed or untrusted messages,
the submitted-line requirement for shell markers, and successful versus failed
exit status.

Related:

- [[presentation-client]]
- [[layout-actor]]
- [[plugin-ui]]
- [[terminal-plugin]]
- [[../experiments/coding-agent-actors]]
