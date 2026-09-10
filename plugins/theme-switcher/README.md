# Theme switcher

Ctrl+Alt+H opens **Themes** in the shared picker. Type to filter, use
arrow keys or hover a row to preview, and press Enter or click to save. Escape
or closing the picker restores the saved theme. An empty result set restores
it too. The shortcut can be changed through the keyboard shortcuts picker.

The default client init starts this presentation-global actor. It owns the
picker request and validates focused and selected stable IDs against the
host's catalogue. Queued requests wait for promotion before previewing;
repeated open actions do not enqueue duplicates. Its host-reserved pane stays outside the shell layout; all interaction uses the
shared picker pane.

The host discovers the full bundled Helix collection and user files from the
`themes/` directory beside the active client configuration. User names override
bundled names. Preview changes remain in memory; selection atomically updates
only the configuration's top-level `theme` value while preserving comments and
other settings. Reloads and previews share the host's theme worker and revision
sequence. External saved-theme changes are retained during a preview and become
the cancellation destination. Invalid themes preserve the last valid display;
failed saves restore the saved theme and report an error in the status bar.

The mailbox protocol used by callers is documented in
[`picker-actor.md`](../../docs/systems/picker-actor.md). Host requests use the
`Theme` variant in [`host_message.rs`](../../crates/brood-wasm/src/host_message.rs).

Verification runs through the built-in `test themes` workload; its CLI parser
is in [`cli.rs`](../../crates/brood-cli/src/cli.rs). The workload drives real
keyboard and pointer input in the offscreen presentation, captures previews and
saved selections, and records client and broodd metrics.
