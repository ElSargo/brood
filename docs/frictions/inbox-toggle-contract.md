---
status: friction
type: design-friction
authority: implementation
last-reviewed: 2026-08-31
---

# Inbox Shortcut Naming No Longer Describes Its Behavior

The product request makes the inbox shortcut reversible, but the current
keybinding catalogue and target contract call the action `inbox.show`. The
implementation accepts both `show` and `toggle` payloads and treats the shipped
shortcut as a toggle so existing persisted bindings remain valid.

The design still needs to decide whether to migrate the public action ID to
`inbox.toggle`, retain `inbox.show` as a compatibility alias, or reserve a
separate always-show action. This is naming and migration policy rather than a
reason to duplicate visibility state in the key policy.
