# Keyboard shortcuts actor

`keybindings` is the presentation-global editor for the default workspace key
policy. `Ctrl+Shift+/` sends the available actions to the shared `picker`
actor. After that actor returns the selected action, this plugin opens a small
modal that captures only the replacement physical chord. It does not implement
its own filtering, result list, or picker navigation.

The capture backdrop preserves the workspace context while containing pointer
input; clicking it or pressing Escape closes the capture. A valid physical
chord changes the global binding immediately and returns to the shared action
picker so several bindings can be edited in one invocation.

Overrides are validated and persisted by the presentation client. Assigning a
chord that is already in use swaps the two actions, so a remap cannot silently
make another action unreachable.
