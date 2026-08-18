# Coding agent actor workload

This plugin is a vertical slice of the target Brood agent architecture. The
default session init actor preserves its terminal startup behavior while also
creating independently addressed coding-agent actors. The default client init
actor preserves the session switcher while creating separate, session-scoped
canvas UI actors after a picker selection. Each UI subscribes to its owning
session's registry and to each selected agent through ordinary actor messages.

Each agent owns its model and reasoning configuration, turn state, subscribers,
and event log. A private worker runs one Codex turn through the capability-
scoped subprocess/stream API while the actor dispatcher remains responsive.
Output is projected back into actor events with a generation-based notification
and streamed to the presentation actor over the bidirectional realm mailbox.

The UI supports:

- `Ctrl+Alt+A` globally to fuzzy-pick a running agent in the current session
  and open a UI focused on it; the selected agent is rendered immediately from
  the launch binding while the full registry snapshot reconnects;
- `Enter` to submit the prompt;
- `Ctrl+V` to attach an image from the presentation clipboard (ordinary text
  is pasted into the prompt);
- desktop drag and drop to attach PNG, JPEG, WebP, or GIF files;
- `Backspace` on an empty prompt to remove the last staged image;
- `Tab` to switch agent;
- `F2` to create another independent agent actor;
- `F6` to cycle model selection;
- `F7` to cycle reasoning effort;
- `Escape` to cancel the active turn.

Every UI's top bar follows the complete session registry, including agents
created from other UI panes. Closing a UI pane does not stop its session-side
agents.

Codex turns launched from this pane automatically receive the
`brood_computer_use` MCP tools bound to the pane's Brood session and agent
identity. No per-user MCP setup is required. Ask the agent normally, for
example: `Use SpeedCrunch to calculate (12345*6789)-987654 and verify the
result visually.` The agent can start a task, inspect screenshots, act through
the compositor's normal input paths, and stop the task. If the application is
not open, it can launch it through Brood's hosted-application path and wait for
its managed window before starting automation. It cannot target the host
desktop, another Brood session, or client-local plugin panes.

Attachments are read by the presentation client and transferred as bounded,
ordered actor messages. The backend never dereferences a client pathname, so
the same interaction works when the selected session is remote. The agent
validates at most four images, 8 MiB each and 12 MiB in aggregate, then gives
the provider temporary workspace files that are removed after the turn. Staged
images are shown as aspect-preserving thumbnails in a strip directly above the
prompt editor until the turn is submitted or the attachment is removed.

This is deliberately an actor-system workload rather than a new privileged
agent subsystem. Editors, diff tools, LSP bridges, automation actors, and other
agents can use the same mailbox protocol and subscribe to the same agent state.
