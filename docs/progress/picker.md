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
