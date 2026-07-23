---
status: target
type: system
authority: product
last-reviewed: 2026-07-13
---

# Input And Keyboard Shortcuts

The target input model keeps final viewport key routing in the presentation client while allowing privileged presentation plugins to participate in immediate keyboard policy through local policy providers. Keys consumed by local UI or actors do not cross the backend connection; keys forwarded to hosted applications are sent under the current presentation lease.

## Ownership

The presentation client owns:

- physical key state and modifier tracking
- focused surface/widget selection
- reserved escape and recovery shortcuts
- keymap conflict resolution
- action invocation
- forwarding keys under the session presentation lease to managed client panes
- suppressing or replaying modifier events as needed

Plugins may contribute:

- action and command registrations
- default shortcut suggestions
- contextual enablement predicates
- privileged key policy providers
- actor-handled command behavior after invocation

## Default Shortcut Model

Most plugins should register actions, not observe raw keyboard input.

```text
plugin registers Action { id, title, scope, default_binding, target_actor }
user and client keymap resolve conflicts
client matches key input in the current focus/context
client sends InvokeAction to the owning actor or executes a client command
```

User configuration and client-reserved bindings override plugin defaults. Conflicts should be visible in diagnostics and configuration UI.

## Routing Order

The target routing order is:

1. client-reserved safety and recovery keys
2. focused native widget/text-input behavior
3. granted local key policy provider for the active context
4. client keymap/action dispatch
5. lease-authorized forwarding to the focused managed client pane

The exact order may be specialized for modes, but raw keyboard observation remains a separate capability. Ordinary plugins should not become global keyloggers.

## Key Policy Provider

A key policy provider is for modal or workspace-level behavior that must decide immediately whether a key is consumed, transformed, or forwarded.

```text
KeyQuery {
  key,
  state,
  modifiers,
  input_mode,
  focus_context,
  pressed_modifier_state,
}
+ latest plugin publication frame
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

The provider may read its latest published state and return a message batch, but it does not mutate client or backend state or wait for actor work. The local client host applies accepted directives and enqueues messages only after the call returns.

The presentation client constructs one typed invocation context before
crossing into the WASM runtime. Its key query groups the code, press/release
state, modifier bitset, and input mode; the same invocation carries an optional
immutable publication lease. The core-module adapter alone lowers the two
bounded guest buffers to the raw pointer/length scalars required by the current
Wasmtime function ABI. Input routing and policy logic do not pass those event
fields as parallel arguments.

The checked-in default-workspace plugin uses this boundary today: its
`WasmGlobalKeyPolicy` returns a bounded decision envelope and actor-message
batch after reading the latest immutable client pane, focus, and application
identity snapshot. The client validates and applies local actions only after the
provider returns. Managed application and session mutations are queued to the
backend asynchronously, so the synchronous key decision never waits for DBus or
network work. The built-in daemon presenter retains the same provider host only
as a compatibility path.

The default policy keeps the normal launch usable without desktop-global
shortcuts or wrapper arguments:

- `Ctrl+Alt+M` focuses the visible session switcher, hides it when invoked
  again while focused, and restores it on the next invocation;
- `Ctrl+J` and `Ctrl+K` cycle focus through the presentation's managed
  windows;
- `Ctrl+O` queues a new `foot` window in the active surface session.

These chords are guest policy. The client owns only validation, local focus and
plugin-pane visibility, and the bounded backend command queue.

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
