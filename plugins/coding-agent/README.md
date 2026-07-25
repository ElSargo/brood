# Coding agent actor workload

This plugin is a vertical slice of the target Manifold agent architecture. The
default session init actor preserves its terminal startup behavior while also
creating independently addressed coding-agent actors. The default client init
actor preserves the session switcher while also creating separate canvas UI
actors. Each UI subscribes to the session registry and to each selected agent
through ordinary actor messages.

Each agent owns its model and reasoning configuration, turn state, subscribers,
and event log. A private worker runs one Codex turn through the capability-
scoped subprocess/stream API while the actor dispatcher remains responsive.
Output is projected back into actor events with a generation-based notification
and streamed to the presentation actor over the bidirectional realm mailbox.

The UI supports:

- `Ctrl+Alt+A` globally to create and focus another UI actor;
- `Enter` to submit the prompt;
- `Tab` to switch agent;
- `F2` to create another independent agent actor;
- `F6` to cycle model selection;
- `F7` to cycle reasoning effort;
- `Escape` to cancel the active turn.

This is deliberately an actor-system workload rather than a new privileged
agent subsystem. Editors, diff tools, LSP bridges, automation actors, and other
agents can use the same mailbox protocol and subscribe to the same agent state.
