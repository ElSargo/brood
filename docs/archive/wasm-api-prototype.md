# WASM API Prototype

This archived page is a focused target-design note for an earlier WASM API spike.

Use [Plugin Runtime](../targets/systems/plugin-runtime.md), [Input And Keyboard Shortcuts](../targets/systems/input-shortcuts.md), [Layout Actor](../targets/systems/layout-actor.md), and [Plugin UI And Pane Rendering](../targets/systems/plugin-ui.md) for the current target. Use [WASM Integration](wasm-integration.md) only as historical context for the broader app and module model.

This note scopes the first WASM API spike to workspace navigation, managed app launch, and keyboard shortcut handling. These are deliberately small because they already exist in the native prototype as focus commands, hosted app spawning, browser launch control, and normal-mode key handling.

The goal is to prove the API boundary before embedding a full app model. WASM should request workspace commands and register command bindings; Brood should execute those requests through the same session process paths used by native input and control commands.

The current execution split is documented in [Plugin Runtime](../targets/systems/plugin-runtime.md): actors provide public isolation and mailbox addressing, actor-scoped workers provide shared-memory concurrency and blocking-looking WASI, and bounded pre-commit policy hooks read immutable published state.

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
- `split-active-right`
- `split-active-down`
- `split-active-smart`
- `remove-active-split`
- `focus-next-split`
- `focus-previous-split`
- `attach-presentation`
- `detach-presentation`

Initial events:

- `pane-opened`
- `pane-closed`
- `pane-focused`
- `presentation-attached`
- `presentation-detached`

The first native implementation can map these to the existing `cycle_focus`, `close_active_window`, `toggle_managed_fullscreen`, `request_attach`, and `request_detach` paths. The API should still use pane/workspace names rather than Wayland window names so the same contract can later apply to WASM panes, browser panes, and other managed app surfaces.

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

Keyboard shortcuts should be command bindings. A WASM controller registers bindings, and Brood dispatches matched key chords to commands when input policy allows interception. Privileged raw or modal key policy belongs in a key policy provider, not an arbitrary actor queue.

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

The first implementation should use `normal` mode only. This matches the current normal-mode handling where keys are consumed by Brood and not forwarded to the focused Wayland client. `passthrough` and `global` are useful later, but they need conflict handling and clearer user expectations.

Shortcut dispatch should produce a normal workspace command request. For example, a WASM controller can bind `j` in normal mode to `focus-next`, or bind `w` to `run-managed-app({ argv: ["wezterm"] })`. The controller should not receive every raw key event unless it has a separate input-observation capability.

Simple shortcut bindings can dispatch directly to workspace commands on the fast path. Controller-backed bindings should emit a shortcut event to the apolicy provider and may request follow-up commands after the initial host-owned decision.

Capability requirements:

- `shortcuts:register` to create bindings.
- `shortcuts:override` to replace an existing binding with equal or higher priority.
- `input:observe-keyboard` only for future raw key observation.

## Event Stream And Fast-Path Publication

The shortcut API should not force every sync key policy handler to replay workspace events. Plugins should be able to process events asynchronously, publish a compact latest-state payload, and let the sync hook read that payload when a shortcut is pressed.

Initial workspace events for navigation policy:

- `pane-opened`
- `pane-closed`
- `pane-focused`
- `pane-client-input`
- `pane-metadata-changed`
- `active-session-changed`

`pane-client-input` means input that Brood actually forwarded to a client pane, such as keyboard, pointer button, or axis input. It should not be emitted for Brood-consumed shortcut keys, focus-only changes, or pointer motion that is not delivered as client input.

The host should expose a current-state snapshot plus ordered event polling:

```text
workspace-snapshot() -> snapshot
poll-workspace-events(after-seq, max-events) -> event-batch
```

The snapshot is the recovery and reload path. The event stream is the incremental update path. If an event batch overflows or the plugin reloads, the plugin rebuilds from `workspace-snapshot`.

For policy-provider state, the host should expose plugin-scoped publication slots:

```text
publish(name: option<string>, payload) -> generation
read-published(root, name: option<string>) -> option<published-record>
```

The unnamed slot is the plugin's default slot. Named slots let multiple async workers publish independent state without coordinating through plugin-managed shared memory.

For example, the default navigation plugin can process pane events asynchronously and publish:

```text
NavState {
  terminal-target: option<pane-id>,
  agent-target: option<pane-id>,
  editor-target: option<pane-id>,
  git-target: option<pane-id>,
}
```

The sync key policy hook then receives the current publication root and resolves `Ctrl+O`, `Ctrl+Enter`, `Ctrl+E`, or `Ctrl+G` by reading this latest state. If the target pane is absent or stale, it can request `run-managed-app` instead.

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

The command executor should be the only layer that calls current `brood` implementation details. WASM, native controllers, DBus control endpoint commands, and tests should converge on this command model as it matures.

## WIT Sketch

Once the Rust command model survives the first spike, the WIT can mirror the same concepts:

```wit
package brood:workspace@0.1.0;

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
    split-active-right,
    split-active-down,
    split-active-smart,
    remove-active-split,
    focus-next-split,
    focus-previous-split,
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

- the exact run arguments are defined in `crates/broodd/src/main.rs`
- config-loaded startup modules are parsed in `crates/broodd/src/config.rs`
- the checked-in example module path lives in `brood.toml`

The guest should export the current v2 key handler entry point:

```text
on_key_v2(key_code: i32, key_state: i32, input_mode: i32, ctrl_pressed: i32, modifiers: i32) -> i32
```

The older v1 entry point remains supported as a compatibility fallback:

```text
on_key(key_code: i32, key_state: i32, input_mode: i32, ctrl_pressed: i32) -> i32
```

ABI values:

- `key_state`: `0` released, `1` pressed
- `input_mode`: `0` passthrough, `1` normal
- `modifiers`: bitset where `1` is Ctrl and `2` is Alt
- return `0` for unhandled, non-zero for handled

The host imports these functions from module `brood`:

```text
focus_next()
focus_previous()
toggle_fullscreen()
close_active_window()
focused_pane_id() -> i64
pane_for_app_id(app_id_ptr: i32, app_id_len: i32) -> i64
focus_pane(pane: i64) -> i32
spawn_app(ptr: i32, len: i32) -> i32
focus_or_spawn_app(app_id_ptr: i32, app_id_len: i32, command_ptr: i32, command_len: i32) -> i32
```

`spawn_app` reads a UTF-8, whitespace-split command from exported guest memory and launches it as a hosted Wayland app with the session `WAYLAND_DISPLAY`.

`focused_pane_id` returns the currently focused content pane, or `-1` when no
content pane owns focus, such as when presentation UI or the sidebar is focused.
`pane_for_app_id` lets the guest map its own app-id model to a live pane in the
active surface session. `focus_pane` requests exact pane focus; native fallback
layout treats this as selecting that pane in the current split slot. A guest that
owns navigation policy should build focus-or-spawn behavior from these primitive
calls so it can update its own model before requesting host focus. The older
`focus_or_spawn_app` import remains a compatibility helper.

When a handler is loaded, normal-mode key presses go to `on_key` first. Modified shortcut chords can also be claimed from passthrough mode, which lets workspace-level bindings work without toggling modes first. A successful normal-mode call replaces the built-in normal-mode map for that key path; if the guest traps, Brood logs the failure and falls back to the built-in handler so the session remains usable during experiments.

The sample handler lives in `examples/wasm-key-handler` and maps a minimal workspace keymap around app-ID-targeted Foot panes:

- `Ctrl-J` / `Ctrl-K` -> focus next / previous pane
- `Ctrl-L` / `Ctrl-H` / `Ctrl-S` -> split right, split down, or smart split
- `Ctrl-U` -> remove the active split
- `Ctrl-.` / `Ctrl-,` -> focus next / previous split slot
- `Ctrl-G` -> focus or spawn the LazyGit pane
- `Ctrl-Enter` -> focus or spawn the pi agent pane
- `Ctrl-1` through `Ctrl-5` -> focus or spawn numbered terminal panes

For the example build/run flow, see `examples/wasm-key-handler/README.md`.

## Validation

The first spike should be tested with real workload behavior:

- Start `brood`.
- Launch two hosted apps through `run-managed-app`.
- Register normal-mode shortcuts for focus next, focus previous, and launch app.
- Send real key input into the presentation window.
- Verify focus changes, spawned windows, shortcut counters, command counters, and existing input-to-frame metrics through `status-json`.

This keeps the WASM API prototype connected to user-visible behavior rather than just unit-level ABI checks.
