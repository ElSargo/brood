# Picker actor

`picker` is a reusable client-realm actor that owns one native plugin pane. The
pane is omitted from layout until a request becomes active, then the client init
composition places it as a centered floating overlay and focuses it.

Requests may open in a loading state. The picker remains interactive while an
animated spinner is visible, and the owner completes discovery with a
`picker-update` snapshot.

Callers address the well-known local actor ID `picker` through ordinary actor
mailboxes. A create request receives a `picker-created` reply with a monotonic
`picker` ID. The ID remains valid while the request is active or queued, so the
owner can append streamed results with `picker-add`. Every create request after
the active one joins a bounded FIFO queue regardless of sender. Queued creation
replies include a one-based `queue_position`; a full queue rejects new work
without allocating an ID.

Items may be strings or JSON objects with a string `label`, optional string
`detail`, and arbitrary caller-owned fields. Selection returns the original item
as `result` and its zero-based append-order `index`; filtering and ranking never
change that index.

The query supports insertion-point editing with Left, Right, Home, End,
Backspace, and Delete. Matched label characters are highlighted, hovering a row
moves keyboard selection to it, and click release activates only the row where
the primary-button press began.

The protocol and ownership rules are documented in
[`docs/systems/picker-actor.md`](../../docs/systems/picker-actor.md). The default
session-switcher actor is the first real caller: `Ctrl+Alt+P` opens a fuzzy
session picker and activates the selected result.
