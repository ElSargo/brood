---
status: target
type: system
authority: product
last-reviewed: 2026-08-09
---

# Input And Keyboard Shortcuts

The target input model keeps final viewport key routing in the presentation
client. One replaceable, privileged key-policy lease holder sequences immediate
global keyboard decisions; any number of plugins contribute lifecycle-bound
actions and binding suggestions without executing on every physical key. Keys
consumed by local UI or actors do not cross the backend connection; keys
forwarded to hosted applications are sent under the current presentation lease.

## Ownership

The presentation client owns:

- physical key state and modifier tracking
- focused surface/widget selection
- reserved escape and recovery shortcuts
- keymap conflict resolution
- the host-owned action registry and active input scopes
- the exclusive key-policy lease and temporary capture lease
- action invocation
- forwarding keys under the session presentation lease to managed client panes
- suppressing or replaying modifier events as needed

Plugins may contribute:

- action and command registrations
- default shortcut suggestions
- contextual enablement predicates
- a candidate global key-policy provider
- actor-handled command behavior after invocation

## Default Shortcut Model

Raw input is an ordered, consumable stream rather than a broadcast event. Most
plugins register actions instead of observing it.

```text
plugin registers Action { id, title, scope, default_binding, target_actor }
host retains registration for the owner generation
global key sequencer receives registry + active context publication
sequencer matches arbitrary chords, leaders, modes, or contextual rules
client validates the winner and sends InvokeAction
```

Action scopes include presentation-global, active session, active or visible
layout lease, focused subtree, and focused pane. A registration scoped to a
layout lease becomes inactive when that lease is hidden or revoked.

Plugin bindings are suggestions, not claims over physical keys. User
configuration and client-reserved bindings override plugin defaults. Conflicts
are resolved deterministically rather than by plugin load or response order and
remain visible in diagnostics and configuration UI.

## Routing Order

The target routing order is:

1. client-reserved safety and recovery keys
2. active temporary input capture, when explicitly granted
3. the current global key-policy lease holder
4. focused client-local pane or native widget when forwarded
5. lease-authorized forwarding to the focused managed client pane

The global provider receives focus and input-method context and decides whether
normal focused delivery continues. Raw keyboard observation remains a separate
capability. Ordinary plugins do not become global keyloggers merely by
registering an action.

## Input Authority

Input uses distinct authority objects:

- `KeyPolicyLease` is the exclusive right to sequence global raw-key decisions;
- `ActionRegistration` is a non-exclusive, lifecycle-bound action contribution;
- `InputCaptureLease` is a temporary exclusive interception for a bounded scope;
- the session presentation lease authorizes forwarding to hosted applications.

The host mints and revokes the exclusive leases. An init actor may nominate or
replace the key-policy provider, but the host records the holder and generation.
Provider replacement, revocation, or failure clears partial chord state and
resynchronizes consumed physical presses before the replacement can run.

An input capture is the escape hatch for key-binding editors, command palettes,
accessibility tools, or similar UI that genuinely needs raw input. It is
exclusive for its scope, explicitly activated, bounded, and revocable. Global
pointer, touch, or gesture capture follows the same rule; normal pointer input
continues to use layout hit testing.

A layout interaction zone may nominate its lease holder's pointer policy. The
client chooses the top zone from the presented layout, then makes at most one
bounded synchronous policy call for that event. The result may consume or pass
the event, acquire or release capture, and enqueue actor messages. It cannot
submit layout, create a resize epoch, or wait for drawing. Captured events retain
one interaction ID and monotonically increasing event sequence even after the
pointer leaves the original hit zone; focus loss, invalid authority, or provider
failure releases capture and sends cancellation through the same actor route.

## Key Policy Provider

A key policy provider is the stateful global sequencer for modal or
workspace-level behavior that must decide immediately whether a key is consumed,
transformed, or forwarded. It may implement arbitrary chords, leaders, modes,
tap/hold behavior, and contextual navigation. Centralizing sequencing does not
restrict the policy language or require bindings to be hardcoded in the host.

```text
KeyQuery {
  key,
  state,
  modifiers,
  input_mode,
  focus_context,
  pressed_modifier_state,
}
+ latest immutable registry and context publication
    -> KeyPolicyResult
```

Candidate result shape:

```text
KeyPolicyResult {
  decision: Abstain | Consume | Forward | ForwardWithModifiers,
  directives: list<client-validated-directive>,
  messages: list<mailbox-message>,
}
```

The provider may read its latest published state and return a message batch, but
it does not mutate client or backend state, invoke another synchronous plugin,
or wait for actor work. The local client host applies accepted directives and
enqueues messages only after the call returns. There is at most one policy call
per physical key event regardless of the number of registered actions.

The presentation client constructs one typed invocation context before
crossing into the WASM runtime. Its key query groups the code, press/release
state, modifier bitset, and input mode; the same invocation carries an optional
immutable publication lease. The core-module adapter alone lowers the two
bounded guest buffers to the raw pointer/length scalars required by the current
Wasmtime function ABI. Input routing and policy logic do not pass those event
fields as parallel arguments.

The checked-in default-workspace plugin uses this boundary: its
`WasmGlobalKeyPolicy` returns a bounded decision envelope and actor-message
batch after reading the latest immutable client pane, focus, and application
identity snapshot. The client validates and applies local actions only after the
provider returns. Managed application and session mutations are queued to the
backend asynchronously, so the synchronous key decision never waits for DBus or
network work. The session server has no key-policy provider.

The default policy keeps the normal launch usable without desktop-global
shortcuts or wrapper arguments:

- `Ctrl+Shift+/` opens the searchable global keyboard-shortcut editor;
- `Ctrl+Alt+M` focuses the visible session switcher, hides it when invoked
  again while focused, and restores it on the next invocation;
- `Ctrl+Alt+I` reveals and focuses the presentation-global inbox;
- `Ctrl+Alt+A` fuzzy-picks a running agent in the active session and opens a
  session-scoped coding-agent UI for it;
- `Ctrl+Alt+T` asks the client- and session-init actors to create a fresh paired
  terminal presentation and session actor;
- `Ctrl+Alt+E` opens or focuses the active session's client-local Wasm Neovim
  pane;
- `Ctrl+Alt+P` opens the reusable fuzzy session picker;
- `Ctrl+Alt+D` toggles the active session's project-diff pane;
- `Ctrl+Alt+Q` closes the focused managed or plugin pane;
- `Ctrl+J` and `Ctrl+K` cycle focus through the presentation's managed
  windows;
- `Ctrl+O` queues a new `foot` window in the active surface session.

These chords are guest policy. Switcher visibility remains actor-owned:
`Ctrl+Alt+M` sends an ordinary sidebar action to the switcher, which republishes
its root layout fragment. The client validates the resulting layout and focus
request; it has no switcher-specific visibility command.

`Ctrl+Alt+I` similarly sends an ordinary show action to the inbox actor. The
root shell reveals and focuses that presentation-global pane without changing
session state.

The root shell reserves a compact status bar across the bottom of every normal
layout. It always advertises `Ctrl+Shift+/` as the route to the complete
shortcut catalogue and, when width permits, keeps the session switcher, inbox,
and terminal chords visible. The leading status text identifies the active
session and total session count; the compact empty state says `No session`.

## Global Remapping

The presentation-global `keybindings` actor renders the effective action
catalogue in a centered modal floating window over the current workspace. Its
full-viewport layer dims and contains the workspace without changing the
underlying layout; clicking the backdrop or pressing Escape closes it. Search
matches action titles, categories, stable IDs, scopes, and current chords.
Enter or a pointer click starts bounded pane-local capture; the next
non-modifier physical key records a chord and Escape cancels capture.

The editor derives the floating window from its allocated viewport rather than
assuming a desktop size. Narrow or recursively composed presentations preserve
a viewport margin, compact the search field and chord column, reduce the visible
row count, and keep the footer and window border inside the viewport.

The presentation client validates updates from the authenticated editor actor,
applies them to the immutable policy publication used by the next key event,
and persists overrides in `keybindings.json` in Brood's global config directory.
Assigning an occupied chord swaps the two actions instead of leaving a hidden
conflict. The editor itself is an ordinary action, so its launcher can also be
remapped without losing access to the catalogue.

The action metadata and defaults have one source of truth in
`crates/brood-keybindings/src/lib.rs`. The synchronous policy consumes that
catalogue and client-published overrides; the editor consumes the same
catalogue for labels and search. Plugin UI code does not maintain a second
hardcoded binding table.

When a terminal or Neovim pane is focused, ordinary Ctrl chords remain pane
input so shell and Vim bindings retain their normal meaning. Ctrl+Alt product
actions remain available through the global policy.

## Action Registry

The host-owned registry retains stable action identity, owner generation,
display metadata, scope, default binding suggestions, and the target actor or
validated client command. Registration and conflict resolution happen outside
the physical-key hot path. The provider receives a bounded immutable registry
revision and active-context snapshot.

Plugins update dynamic enablement asynchronously. The hot path does not call
plugin-supplied predicates. A custom global policy may ignore suggested
bindings and map the registered action identities using an entirely different
key grammar.

Configuration and plugin defaults share this named action model. Terminal
choice, argument vectors, and similar application policy belong in registered
actions and user configuration rather than compiled-in host chords.

## Actor Follow-Up

When a shortcut invokes long-running behavior, the policy path should only make
the immediate routing decision. Follow-up coordination belongs to an actor; the
actor delegates blocking-looking I/O or shared-state concurrent work to its
workers:

```text
key press
  -> policy provider consumes key and returns message InvokeAction(search.open)
  -> host enqueues message
  -> search actor opens pane or updates UI asynchronously
```

If a key should focus a local pane immediately, the result remains a client-validated directive. Launching or mutating a backend application uses an asynchronous backend command request; the plugin never directly mutates backend process or session state.

## Diagnostics

Input diagnostics should separate client keymap behavior from policy provider and backend forwarding behavior:

- active key-policy and input-capture lease generations
- action-registry and active-context revisions
- matched binding and scope
- winning plugin/action
- conflicts and overrides
- key policy provider call count and latency
- traps, timeouts, disabled providers
- consumed, forwarded, and transformed key counts
- messages emitted from key policy provider results
- lease-authorized hosted-application forwarding and backend rejection

Related:

- [[presentation-client]]
- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[layout-actor]]
- [[extension-host]]
- [[../reference/wasm-fast-lanes]]
