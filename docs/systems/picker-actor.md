---
status: current
type: system
authority: implementation
---

# Picker Actor

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
    {
      "id": "file:README.md",
      "value": {"label": "README.md", "detail": "Project overview", "path": "README.md"}
    },
    {
      "id": "file:docs/architecture.md",
      "value": {"label": "docs/architecture.md", "detail": "Architecture overview", "path": "docs/architecture.md"}
    }
  ]
}
```

Callers that discover items asynchronously set `loading` and may provide a
short `loading_label`. The picker is visible and accepts a query immediately;
it renders a progress spinner until the owner publishes the discovered items.

`request` is optional caller-owned JSON used only for correlating the reply.
Every item is an envelope of `{ "id": string, "value": JSON }`. IDs are
UTF-8 strings of 1–8192 bytes and must be unique within the complete item set.
The inner value is either a string or an object with a string `label`, optional
string `detail`, and any other caller-owned JSON.

The picker immediately allocates a monotonic ID and replies:

```json
{
  "type": "picker-created",
  "request": 41,
  "picker": 7,
  "queued": true,
  "queue_position": 2
}
```

`queue_position` is present only for queued work and is its one-based position
at admission time. The reliable queue is bounded; saturation returns
`picker-error` without allocating an ID.

The ID is stable while the request is active or queued. A caller streams more
results without replacing existing entries:

```json
{
  "type": "picker-add",
  "picker": 7,
  "items": [
    {"id": "file:src/main.rs", "value": {"label": "src/main.rs", "path": "src/main.rs"}}
  ]
}
```

When a discovery result is a fresh snapshot rather than an append-only stream,
the owner replaces the items and completes the loading state in one update.
The complete proposed set is validated transactionally; malformed or duplicate
IDs return `picker-error` and preserve the previous items:

```json
{
  "type": "picker-update",
  "picker": 7,
  "items": [
    {"id": "backend:1", "value": {"label": "Local", "backend": 1}}
  ],
  "loading": false
}
```

On selection, the owner receives the original item and its zero-based append
order, independent of the current fuzzy ranking:

```json
{
  "type": "picker-selected",
  "picker": 7,
  "item_id": "file:src/main.rs",
  "result": {"label": "src/main.rs", "path": "src/main.rs"},
  "index": 2
}
```

`item_id` is the stable identity from the displayed item. `index` is the
zero-based append order for diagnostics and compatibility; callers use
`item_id` and validate the returned value against current domain state.

An owner may cancel active or queued work with `picker-cancel`. Escape produces
the same `picker-cancelled` reply. Invalid messages, unknown IDs, malformed
items, and ownership violations return `picker-error`.

## Queueing and matching

There is one active picker pane. Every create request received while it is
active joins a bounded FIFO queue, including additional requests from the
current owner. Streaming updates address the stable ID and therefore update
queued work without displaying it early. Promotion cancels animation work
leased to the completed picker before starting work for the promoted picker, so
a late worker event cannot mutate its replacement.

Matching is case-insensitive subsequence matching with bonuses for contiguous
characters and word boundaries. Ties retain append order. An empty query shows
all items in append order. Replacing or reordering items preserves the current
selection when its `item_id` remains present; otherwise selection falls back to
the first visible item. The matching characters are highlighted in each
visible label. Query editing has a Unicode-safe insertion point with Left/Right,
Home/End, Backspace, and Delete. Pointer hover moves the same selection used by
the keyboard, while a click activates only after press and release on the same
row. Pixel scroll deltas accumulate before changing rows.

The default session-switcher and keybindings actors are consumers.
`Ctrl+Alt+P` sends a normal `picker-create` message and activates the session
returned in `picker-selected`. `Ctrl+Shift+/` sends the action catalogue to the
same actor; after selection, the keybindings actor owns only physical-chord
capture and persistence.

## Verification

Pure behavior tests cover fuzzy ranking, append-order indices, queued promotion,
and selection identity. The nested picker workload builds the real component,
opens it through the default key policy, sends real text and key input, and
checks the resulting session activation and presentation metrics.

See also [[../targets/systems/layout-actor]], [[../targets/systems/plugin-ui]], and [[../targets/systems/input-shortcuts]].
