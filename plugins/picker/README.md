# Picker actor

`picker` is a reusable client-realm actor that owns one native plugin pane. The
pane is omitted from layout until a request becomes active, then the client init
composition places it as a centered floating overlay and focuses it.

Callers address the well-known local actor ID `picker` through ordinary actor
mailboxes. A create request receives a `picker-created` reply with a monotonic
`picker` ID. The ID remains valid while the request is active or queued, so the
owner can append streamed results with `picker-add`. Every create request after
the active one joins a FIFO queue regardless of sender.

Items may be strings or JSON objects with a string `label`, optional string
`detail`, and arbitrary caller-owned fields. Selection returns the original item
as `result` and its zero-based append-order `index`; filtering and ranking never
change that index.

The protocol and ownership rules are documented in
[`docs/systems/picker-actor.md`](../../docs/systems/picker-actor.md). The default
session-switcher actor is the first real caller: `Ctrl+Alt+P` opens a fuzzy
session picker and activates the selected result.
