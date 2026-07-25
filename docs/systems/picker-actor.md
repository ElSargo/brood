# Picker actor

The presentation realm exposes one reusable actor at the stable local address
`picker`. It owns a native plugin pane, fuzzy matching, keyboard and pointer
selection, request serialization, and result delivery. Callers own the data and
the effect of a selection; the picker does not know how a result will be used.

## Lifecycle and layout

The client init actor starts the picker alongside the normal presentation
actors. The picker pane exists for the actor generation but is absent from the
resolved layout while idle.

When work becomes active, the picker tells the root layout actor to place its
pane as a centered, high-z floating item and focus it. The root retains its
existing `rest` delegation and accepted child composition underneath the
picker. Completing or cancelling the last request removes the pane item and
restores the prior workspace focus. Promoting queued work keeps the same pane
visible so there is no close/open layout flash.

## Mailbox protocol

All protocol messages are JSON sent through ordinary local actor mailboxes.
The host-authenticated mailbox sender is the request owner; a caller cannot
append to or cancel another actor's picker.

A caller creates work by sending:

```json
{
  "type": "picker-create",
  "request": 41,
  "title": "Open file",
  "placeholder": "Type a path…",
  "items": [
    "README.md",
    {
      "label": "docs/architecture.md",
      "detail": "Architecture overview",
      "value": {"path": "docs/architecture.md"}
    }
  ]
}
```

`request` is optional caller-owned JSON used only for correlating the reply.
Items are either strings or objects with a string `label`. Object items may
contain an optional string `detail` and any other caller-owned JSON.

The picker immediately allocates a monotonic ID and replies:

```json
{
  "type": "picker-created",
  "request": 41,
  "picker": 7,
  "queued": false
}
```

The ID is stable while the request is active or queued. A caller streams more
results without replacing existing entries:

```json
{
  "type": "picker-add",
  "picker": 7,
  "items": [
    {"label": "src/main.rs", "value": {"path": "src/main.rs"}}
  ]
}
```

On selection, the owner receives the original item and its zero-based append
order, independent of the current fuzzy ranking:

```json
{
  "type": "picker-selected",
  "picker": 7,
  "result": {"label": "src/main.rs", "value": {"path": "src/main.rs"}},
  "index": 2
}
```

An owner may cancel active or queued work with `picker-cancel`. Escape produces
the same `picker-cancelled` reply. Invalid messages, unknown IDs, malformed
items, and ownership violations return `picker-error`.

## Queueing and matching

There is one active picker pane. Every create request received while it is
active joins a FIFO queue, including additional requests from the current
owner. Streaming updates address the stable ID and therefore update queued work
without displaying it early.

Matching is case-insensitive subsequence matching with bonuses for contiguous
characters and word boundaries. Ties retain append order. An empty query shows
all items in append order.

The default session-switcher actor is the first consumer. `Ctrl+Alt+P` sends a
normal `picker-create` message and activates the session returned in
`picker-selected`.

## Verification

Pure behavior tests cover fuzzy ranking, append-order indices, queued promotion,
and selection identity. The nested picker workload builds the real component,
opens it through the default key policy, sends real text and key input, and
checks the resulting session activation and presentation metrics.

See also [[layout-actor]], [[plugin-ui]], and [[input-shortcuts]].
