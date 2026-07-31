# Keyboard shortcuts actor

`keybindings` is the presentation-global editor for the default workspace key
policy. `Ctrl+Shift+/` opens a floating searchable catalogue. Selecting an
action and pressing a physical chord changes its global binding immediately.

Overrides are validated and persisted by the presentation client. Assigning a
chord that is already in use swaps the two actions, so a remap cannot silently
make another action unreachable.
