---
status: target
type: system
authority: product
last-reviewed: 2026-07-03
---

# Input And Keyboard Shortcuts

The target input model keeps final key routing in the host while allowing privileged plugins to participate in immediate keyboard policy through policy providers.

## Ownership

The host owns:

- physical key state and modifier tracking
- focused surface/widget selection
- reserved escape and recovery shortcuts
- keymap conflict resolution
- action invocation
- forwarding keys to managed client panes
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
user and host keymap resolve conflicts
host matches key input in the current focus/context
host sends InvokeAction to the owning actor or executes a host command
```

User configuration and host-reserved bindings override plugin defaults. Conflicts should be visible in diagnostics and configuration UI.

## Routing Order

The target routing order is:

1. host-reserved safety and recovery keys
2. focused native widget/text-input behavior
3. granted key policy provider for the active context
4. host keymap/action dispatch
5. forwarding to the focused managed client pane

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
  directives: list<host-validated-directive>,
  messages: list<mailbox-message>,
}
```

The provider may read its latest published state and return a message batch, but it does not mutate host state or wait for actor work. The host applies accepted directives and enqueues messages only after the call returns.

The current native prototype handles `Ctrl+O` this way: it reads the `debug-toggle` actor's latest publication frame, returns a host command for `foot htop` or `foot btop`, and returns a mailbox message asking the actor to toggle and publish the next frame.

## Relation To The Current WASM Key Handler

The current `wasm_key` implementation proves that a synchronous key path is useful: the host calls a WASM export during key handling and needs an immediate handled/unhandled answer.

The target keeps that property but changes the boundary:

- current prototype: guest calls side-effect-style imports that append pending host commands
- target model: guest returns a bounded key decision envelope and message batch

This avoids reentrancy and deadlock while preserving fast key handling.

## Actor Follow-Up

When a shortcut invokes long-running behavior, the policy path should only make the immediate routing decision. Follow-up work belongs to actors:

```text
key press
  -> policy provider consumes key and returns message InvokeAction(search.open)
  -> host enqueues message
  -> search actor opens pane or updates UI asynchronously
```

If a key should launch or focus an app immediately, the result should still be a host-validated directive or command request. The host executes the command; the plugin never directly mutates pane focus or process state.

## Diagnostics

Input diagnostics should separate host keymap behavior from policy provider behavior:

- matched binding and scope
- winning plugin/action
- conflicts and overrides
- key policy provider call count and latency
- traps, timeouts, disabled providers
- consumed, forwarded, and transformed key counts
- messages emitted from key policy provider results

Related:

- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[layout-policy]]
- [[extension-host]]
- [[../reference/wasm-fast-lanes]]
