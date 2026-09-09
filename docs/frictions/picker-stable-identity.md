# Picker row identity across immutable input frames

Implementation checkpoint: 2026-09-05. Resolved by explicit user decision.

The pure-renderer target requires input to retain the displayed frame's stable
action identity and forbids reinterpreting an old positional selection against
new rows. See `docs/targets/systems/plugin-pure-renderer.md`, Input Contract.

Picker's current public protocol accepts arbitrary string/object values in
`items`. `PickerEntry` stores a positional `index`; `PickerRequest::update`
replaces entries and reassigns indices. `SelectedReply` returns both the value
and that index. There is no caller-supplied occurrence identity or uniqueness
contract. Duplicate values are allowed. Relevant definitions are in
`plugins/picker/src/lib.rs`: `Message`, `PickerEntry`, `PickerRequest`,
`update_picker`, and `SelectedReply`.

A queued selection against an older displayed frame cannot safely use its index
in the newer list. Content matching does not disambiguate duplicate occurrences.
This is a data-model gap in the migration, not an untrusted-plugin security issue.

Two possible policies need an explicit choice:

- Add caller-supplied stable item IDs, unique within each picker, retained across
  updates; update selection replies and callers to preserve intended identity.
- Define an occurrence by picker ID, item-list revision, and index, rejecting
  stale selections after any replacement of the item list. This preserves the
  existing external shape but intentionally cancels input even after a reorder
  which keeps the desired value present.

No policy has been silently selected. Implementation paused under the user's
instruction to call out major target problems. Existing completed switcher and
native work remains available. A partial, unverified status-bar prototype was
saved as `docs/progress/plugin-pure-renderer-status-bar-prototype.patch`; its
source files were returned to their pre-prototype state for a reviewable
checkpoint. Picker/keybindings and inbox have no migration edits from this pass.

## Resolution

The user chose caller-supplied stable IDs on 2026-09-05. The target now records
this contract, and implementation has resumed. Picker messages use an explicit
`{id, value}` envelope; replies carry `item_id` alongside the original value.
IDs survive reorder and replacement, and duplicate IDs are rejected before
mutating the list. Both bundled callers are being migrated. The paused-state
observations above describe the checkpoint, not the current implementation.
