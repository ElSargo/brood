# Brood Neovim bridge

`brood-nvim` is the real packaged Neovim with a small Brood control bridge
loaded in addition to the user's normal configuration. It does not replace
Neovim's editing model, configuration, plugins, registers, motions, text
objects, or command language.

Open or focus the session editor with `Ctrl+Alt+E`. Inside Neovim:

- `:BroodDiff` opens or focuses the session's diff-viewer pane.
- `:BroodTerminal [command]` opens a new terminal pane and optionally runs the
  command.
- `:BroodAgent [prompt]` sends work to the session coding agent. With no
  argument it asks for a prompt.
- `:BroodMessage {target} {json-or-string}` sends a bounded message to a
  same-session actor. Bare names and `session:` target the session realm;
  `presentation:` targets an actor in the attached presentation. There is no
  host target, and presentation-shell orchestration actors are reserved for the
  typed commands above.

The bridge defines only `<Plug>` mappings. It does not claim a leader or replace
user mappings. A user can opt into bindings such as:

```lua
vim.keymap.set("n", "<leader>gd", "<Plug>(BroodDiff)")
vim.keymap.set("n", "<leader>tt", "<Plug>(BroodTerminal)")
vim.keymap.set("n", "<leader>aa", "<Plug>(BroodAgent)")
```

Messages leave the PTY on the private
`OSC 777;brood;<hex-encoded-JSON>` control lane. The terminal session actor
validates their size and destination, stamps the owning session, and converts
them into ordinary actor messages. User configuration can also call
`vim.brood.send({ ... })`; the same bounds and routing validation still apply
at the session actor.

Incoming `open-editor` messages start the editor if necessary and use its local
Neovim server socket to open the requested path and optional line and column.
Requests are serialized and bounded. They do not start a second editor.

The editor process belongs to the session actor, not to its visible terminal
pane. Closing or disconnecting the presentation pane detaches that view while
the editor, buffers, undo history, and server socket remain alive. Reopening
reattaches one presentation view. Exiting Neovim or closing the Brood session
ends the editor process.

See [[../../docs/systems/editor-plugin]] for the actor and lifecycle design.
