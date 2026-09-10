---
status: current
type: progress
authority: implementation
---

# Picker

## 2026-08-31 interaction and queue hardening

The picker request reducer now rejects admission beyond its bounded reliable
FIFO and reports the one-based admission position for queued work. Spinner
workers carry the picker ID that leased them; completion, cancellation, and
promotion revoke the old lease so a late animation result cannot advance the
new active request.

Query editing now keeps a Unicode scalar insertion point and handles horizontal
movement, Home/End, Backspace, Delete, and insertion. Fuzzy matching retains
the original character positions used by scoring, allowing visible labels to
highlight the actual subsequence. Hover uses the same selection as keyboard
navigation, primary-button activation requires press and release on the same
row, and smooth scroll input accumulates before changing rows.

Focused tests cover Unicode matching and insertion, bounded queue admission,
FIFO promotion, spinner lease replacement, stable result identity, and
pointer-row selection.

## 2026-09-10 focused-item notifications

Create requests can opt into `notify_focus`. The actor publishes a
`picker-focused` message for initial activation, changes of stable focused ID or
value, and transitions to no matches (null item fields). Queued work first
notifies after promotion; finished work emits only its terminal reply. The
notification cache is actor state and is omitted from pure render views.

The theme switcher is the first consumer. Both the legacy presentation path and
pure renderer actor reduction share the focus notifier. Pure facet navigation
previously applied both its displayed-order intent and the actor's Arrow key
handler, skipping a row. It now applies the intent once and scrolls that focused
item into view.

Verification includes focused identity/value changes, no-match transitions,
queued promotion, cancellation and selection, plus real keyboard and pointer
input in the offscreen theme workload. The protocol is in
[`picker-actor.md`](../systems/picker-actor.md).
