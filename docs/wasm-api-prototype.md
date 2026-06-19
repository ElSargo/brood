# WASM API Prototype

This page is a focused target-design note for the first WASM API spike.

Use [WASM Integration](wasm-integration.md) as the canonical overview. This page only narrows the first command/API slice.

This note scopes the first WASM API spike to workspace navigation, managed app launch, and keyboard shortcut handling. These are deliberately small because they already exist in the native prototype as focus commands, hosted app spawning, browser launch control, and normal-mode key handling.

The goal is to prove the API boundary before embedding a full app model. WASM should request workspace commands and register command bindings; Manifold should execute those requests through the same session process paths used by native input and control commands.

The execution lane split for these APIs is documented in [WASM Execution Model](wasm-execution-model.md): normal plugin work runs asynchronously, while only bounded pre-commit policy hooks and fast shortcut decisions run synchronously.

## First Prototype Order

1. Navigation commands.
2. Running managed apps.
3. Keyboard shortcut registration and dispatch.

This order keeps risk low. Navigation can be tested without creating new producer state. App launch then proves capability checks and lifecycle events. Keyboard shortcuts finally prove that WASM can participate in input policy without receiving raw compositor authority.

## Workspace Navigation

Navigation should be modeled as workspace commands, not direct focus mutation.

Initial commands:

- `focus-next`
- `focus-previous`
- `focus-pane(pane-id)`
- `close-focused-pane`
- `toggle-focused-fullscreen`
- `attach-presentation`
- `detach-presentation`

Initial events:

- `pane-opened`
- `pane-closed`
- `pane-focused`
- `presentation-attached`
- `presentation-detached`

The first native implementation can map these to the existing `cycle_focus`, `close_active_window`, `toggle_managed_fullscreen`, `request_attach`, and `request_detach` paths. The API should still use pane/workspace names rather than Wayland window names so the same contract can later apply to WASM panes, browser panes, and WSLg-native managed app surfaces.

## Running Managed Apps

WASM should not receive generic process spawning by default. The first app-running API should launch managed apps through session policy so launched surfaces become pane producers.

Initial command:

```text
run-managed-app(spec) -> producer-id
```

Initial `ManagedAppSpec` fields:

- `argv`
- `cwd`
- `env`
- `title-hint`
- `app-id-hint`
- `presentation-request`

`argv` is required. `cwd` and `env` default to the session environment. `presentation-request` can ask for placement, focus, fullscreen, or grouping, but the presentation surface remains free to adapt or reject the request.

Capability requirements should start narrow:

- `apps:run` for launching a declared managed app.
- `apps:run-argv` for arbitrary argv launch.
- `session:env-read` for inheriting or inspecting the session environment.
- `session:env-write` for mutating launch defaults.

For the first spike, `run-managed-app` can call the current hosted Wayland spawn path and emit lifecycle events when windows appear through the existing compositor mapping path. Browser-specific launch can remain a native control API until the general managed app command needs browser options.

## Keyboard Shortcuts

Keyboard shortcuts should be command bindings. A WASM controller registers bindings, and Manifold dispatches matched key chords to commands when input policy allows interception.

Initial host functions:

```text
register-shortcut(binding, command)
unregister-shortcut(binding-id)
list-shortcuts() -> list<shortcut>
```

Initial binding fields:

- `id`
- `scope`
- `key`
- `modifiers`
- `mode`
- `priority`
- `when`

Scopes:

- `workspace`
- `session`
- `pane`
- `app`

Modes:

- `normal`
- `passthrough`
- `global`

The first implementation should use `normal` mode only. This matches the current normal-mode handling where keys are consumed by Manifold and not forwarded to the focused Wayland client. `passthrough` and `global` are useful later, but they need conflict handling and clearer user expectations.

Shortcut dispatch should produce a normal workspace command request. For example, a WASM controller can bind `j` in normal mode to `focus-next`, or bind `w` to `run-managed-app({ argv: ["wezterm"] })`. The controller should not receive every raw key event unless it has a separate input-observation capability.

Simple shortcut bindings can dispatch directly to workspace commands on the fast path. Controller-backed bindings should emit a shortcut event to the async lane and may request follow-up commands after the initial host-owned decision.

Capability requirements:

- `shortcuts:register` to create bindings.
- `shortcuts:override` to replace an existing binding with equal or higher priority.
- `input:observe-keyboard` only for future raw key observation.

## Rust Trait Sketch

The native Rust API can prove command semantics before WIT is frozen:

```rust
pub trait WorkspaceHost {
    fn request_command(&mut self, actor: ActorId, command: WorkspaceCommand) -> CommandResult;
    fn subscribe(&mut self, actor: ActorId, events: EventFilter) -> SubscriptionId;
}

pub enum WorkspaceCommand {
    FocusNext,
    FocusPrevious,
    FocusPane(PaneId),
    CloseFocusedPane,
    ToggleFocusedFullscreen,
    AttachPresentation,
    DetachPresentation,
    RunManagedApp(ManagedAppSpec),
    RegisterShortcut(ShortcutBinding, Box<WorkspaceCommand>),
    UnregisterShortcut(ShortcutBindingId),
}
```

The command executor should be the only layer that calls current `manifoldd` implementation details. WASM, native controllers, control socket commands, and tests should converge on this command model as it matures.

## WIT Sketch

Once the Rust command model survives the first spike, the WIT can mirror the same concepts:

```wit
package manifold:workspace@0.1.0;

interface commands {
  type pane-id = string;
  type producer-id = string;
  type binding-id = string;

  enum focus-direction {
    next,
    previous,
  }

  record managed-app-spec {
    argv: list<string>,
    cwd: option<string>,
    env: list<tuple<string, string>>,
    title-hint: option<string>,
    app-id-hint: option<string>,
  }

  enum command {
    focus-next,
    focus-previous,
    close-focused-pane,
    toggle-focused-fullscreen,
    attach-presentation,
    detach-presentation,
  }

  run-managed-app: func(spec: managed-app-spec) -> result<producer-id, string>;
  request: func(command: command) -> result<_, string>;
}

interface shortcuts {
  record shortcut-binding {
    id: string,
    scope: string,
    key: string,
    modifiers: list<string>,
    mode: string,
    priority: u32,
    when: option<string>,
  }

  register-shortcut: func(binding: shortcut-binding, command: string) -> result<binding-id, string>;
  unregister-shortcut: func(id: binding-id) -> result<_, string>;
}
```

The `command: string` placeholder in `register-shortcut` should become a typed command reference once the command representation is settled.

## Loaded Key Handler Prototype

The first runnable spike uses a narrow core-WASM ABI instead of WIT.

Current usability note:

- the exact run arguments are defined in `crates/manifoldd/src/main.rs`
- config-loaded startup modules are parsed in `crates/manifoldd/src/config.rs`
- the checked-in example module path lives in `manifold.toml`

The guest exports:

```text
on_key(key_code: i32, key_state: i32, input_mode: i32, ctrl_pressed: i32) -> i32
```

ABI values:

- `key_state`: `0` released, `1` pressed
- `input_mode`: `0` passthrough, `1` normal
- return `0` for unhandled, non-zero for handled

The host imports these functions from module `manifold`:

```text
focus_next()
focus_previous()
toggle_fullscreen()
close_active_window()
spawn_app(ptr: i32, len: i32) -> i32
```

`spawn_app` reads a UTF-8, whitespace-split command from exported guest memory and launches it as a hosted Wayland app with the session `WAYLAND_DISPLAY`.

When a handler is loaded, normal-mode key presses go to `on_key` first. A successful call replaces the built-in normal-mode map for that key path; if the guest traps, Manifold logs the failure and falls back to the built-in handler so the session remains usable during experiments.

The sample handler lives in `examples/wasm-key-handler` and currently maps normal-mode chords:

- `Ctrl-O` -> spawn `kitty`
- `Ctrl-Q` -> `close_active_window`

For the example build/run flow, see `examples/wasm-key-handler/README.md`.

## Validation

The first spike should be tested with real workload behavior:

- Start `manifoldd`.
- Launch two hosted apps through `run-managed-app`.
- Register normal-mode shortcuts for focus next, focus previous, and launch app.
- Send real key input into the presentation window.
- Verify focus changes, spawned windows, shortcut counters, command counters, and existing input-to-frame metrics through `status-json`.

This keeps the WASM API prototype connected to user-visible behavior rather than just unit-level ABI checks.
