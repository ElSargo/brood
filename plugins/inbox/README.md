# Inbox presentation actor

The inbox is one presentation-global actor owned by the client init actor. It
renders a bounded queue of completion notifications independently of the
active backend or surface session.

Session actors send `inbox-push` messages with a short category, title, detail,
and the presentation actor that should receive activation. Clicking an item
removes it from the queue and sends that actor a `focus` message. Pane IDs stay
inside the target presentation actor, so notification routing does not depend
on backend-local pane numbers.

The default client starts the inbox rail hidden and toggles it on `Ctrl+Alt+I`.
Escape or the header close control hides it, arrow keys (or `j`/`k`) move the
keyboard selection, Enter opens the selected source, and Delete or Backspace
dismisses it. The queue remains live while hidden and intentionally disappears
when that client disconnects; agent and terminal ownership remains in the
backend session.
