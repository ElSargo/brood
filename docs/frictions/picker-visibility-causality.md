---
status: friction
type: design-friction
authority: implementation
last-reviewed: 2026-08-31
---

# Picker Visibility Notifications Lack Intent Identity

Picker creation and selection replies identify the picker, so the session
switcher can reject a stale backend-discovery result. The separate
`picker-pane` visibility notification carries only the reusable pane and a
boolean. If one queued picker closes while another is promoted, a delayed
visibility message cannot be attributed to either picker intent by the shell.

The picker actor currently emits these messages in mailbox order, which limits
the practical race, but that ordering is not a causal admission rule across a
future transport or worker boundary. The visibility notification should carry
the active picker identity (and ideally the owner's creation intent) before
the system claims permutation-proof modal lifecycle behavior.
