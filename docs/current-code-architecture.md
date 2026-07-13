# Current Code Architecture, Data Flow, and Control Flow

This document describes the architecture that exists in the code today. It is
based on direct inspection of `crates/manifoldd`, `crates/manifold-wasm`, the
Rust sidecar binaries, and the checked-in GTK session switcher application. It
intentionally describes current behavior rather than a target design.

## 1. High-level shape

Manifold currently consists of two Rust workspace packages, `manifoldd` and
`manifold-wasm`, plus support applications, plugins, examples, and scripts. The
main runtime is a nested Wayland compositor with an optional presentation
frontend. It hosts Wayland clients on an internal Wayland socket, exposes
control APIs over DBus, can attach or detach a visible/offscreen presentation
target, and keeps most mutable compositor state inside one central `Manifold`
state object.

At runtime, the core process combines these roles:

- **Wayland compositor/server**: accepts nested Wayland clients, implements Smithay compositor, xdg-shell, seat, data device, output, dmabuf, and explicit-sync handler traits.
- **Window/session manager**: assigns each top-level surface a pane id and a surface-session id, tracks focus, remembers per-directory session defaults, and maintains native or WASM-provided layout state.
- **Presentation renderer**: renders hosted Wayland surfaces into a Winit/EGL window or an offscreen EGL renderbuffer.
- **Input router**: translates presentation keyboard/mouse/scroll events into nested Wayland seat events and offers keys to the synchronous WASM global-key policy before forwarding them to the focused client.
- **DBus control service**: serves token-oriented control commands and typed DBus interfaces. DBus handler tasks marshal requests back to the compositor thread before mutating state.
- **Session-local DBus bus provider**: can start a tiny in-process DBus broker per surface session for hosted clients, including limited forwarding of desktop notifications to the host session bus.
- **Process supervisor for hosted apps**: launches managed Wayland apps, presentation helper apps, browser panes, and test subjects; tracks child pids/exits.
- **WASM/plugin host**: hot-loads core-module actors and key policy providers,
  Component Model actors, marked Preview 1 compatibility actors, and async test
  harnesses, and starts a small built-in plugin actor runtime.

The most important architectural fact is that **`Manifold` is the actor-like state container for nearly everything**. The compositor thread owns it and mutates it from calloop callbacks. Async subsystems do not take direct mutable references; they communicate through channels and one-shot response senders.

## 2. Source/module map

| Area | Primary files | Current responsibility |
| --- | --- | --- |
| CLI entry and argument model | `lib.rs`, `main.rs` | Clap command tree, default command dispatch, environment constants, session CLI entry. |
| Compositor runtime | `compositor.rs` | Tokio/calloop startup, Smithay handler implementations, nested-test orchestration, dmabuf import path. |
| Central state | `state.rs` | Wayland/Smithay state, presentation state, sessions, panes, focus, layout selection, extensions, agents, status, stats, child processes, browser manager, plugins, DBus bus handles. |
| Presentation | `presentation.rs`, `winit_presenter.rs` | Winit/offscreen event source, EGL/GLES backend, rendering, frame callbacks, dmabuf advertisement, attach/detach state. |
| Input | `input.rs` | Keyboard and pointer routing from presentation/native events into Wayland seat events plus shortcut interception. |
| Layout | `layout.rs` | Native split-tree layout, resolved normalized layout model, hit-testing, layout actions/keymap. |
| Control plane | `control.rs` | DBus service/proxies, CLI control helpers, token command interpreter, status snapshots, inject/debug commands. |
| Config and runner | `config.rs`, `session_runner.rs`, `launch_context.rs`, `src/bin/*runner*`, `src/bin/manifold-sessiond.rs` | Config loading, startup modules/apps, project session runner discovery, systemd scope launch records, sidecar activation. |
| DBus isolation | `session_bus.rs` | In-process DBus bus per session, DBus auth/message router, name registry, host notification proxy. |
| WASM core plugins and host integration | `crates/manifold-wasm/src/module.rs`, `crates/manifold-wasm/src/actor.rs`, `crates/manifold-wasm/src/global_key_policy.rs`, `crates/manifold-wasm/src/policy_provider.rs`, `state.rs`, `wasm_test.rs` | Core-module actor/policy discovery and execution, daemon artifact routing/lifecycle, and the async test harness. |
| WASM actor/worker runtime | `crates/manifold-wasm/src/component_actor.rs`, `crates/manifold-wasm/src/preview1_worker.rs`, `crates/manifold-wasm/src/completion_io.rs`, `crates/manifold-wasm/src/coordination.rs` | Component Model actors, the Preview 1 POSIX compatibility actor/worker runtime, completion I/O, and shared notification/pollable coordination. |
| Plugins | `plugin_runtime.rs` | Built-in actor, host-mailbox, and publication scaffold used by the DBus plugin surface. |
| Browser | `browser.rs` | Chromium process manager and DevTools HTTP helper under control commands. |
| Diagnostics | `debug_dump.rs` | Status/composed/surface capture bundle. |
| External sidebar app | `apps/manifold-session-switcher.py` | GTK/Libadwaita DBus client, session switcher, sidebar registration, agent/status UI, notifications. |

## 3. Process and thread topology

### 3.1 Binaries and entry points

The package defines these Rust binaries:

- `manifold` (`src/main.rs`) calls `manifoldd::run_cli()`.
- `manifold-sessiond` calls `manifoldd::run_session_cli()` and is used by project session runners.
- `manifold-runner-nix-develop` activates/caches `nix print-dev-env` and then runs an inner binary.
- `manifold-runner-exec` reads a launch record and `exec`s a recorded command inside a runner environment.

`run_cli()` initializes tracing, parses `Args`, and dispatches to the selected command. If no subcommand is supplied, it behaves as `run` with default arguments. The `run` command enters `compositor::run`, while most non-runtime commands go through `control.rs` helpers.

`run_session_cli()` is a runner-oriented entry point. It first tries `launch_context::current_launch_record()`. If the process is inside a Manifold launch systemd scope, it extracts `SessionArgs` from the launch record. If no launch record exists, it parses `SessionArgs` directly. It then calls `compositor::serve_session()`.

### 3.2 Runtime thread model

`serve_session()` creates a multi-threaded Tokio runtime and then spawns a named `manifold-calloop` thread. The calloop thread runs `serve_session_on_calloop_thread()`, creates `EventLoop<'static, Manifold>`, creates the Wayland `Display<Manifold>`, initializes `Manifold`, registers event sources, and runs the main event loop.

The Tokio runtime remains alive around the calloop thread. It hosts:

- zbus DBus service tasks,
- blocking spawned child-process launch/wait tasks,
- plugin actor tasks,
- Component actor dispatcher/worker/completion tasks and Preview 1 worker
  Stores,
- in-process DBus accept/client tasks when a session bus is started through the Tokio runtime,
- test harness blocking tasks.

The mutable compositor state is not shared directly with those tasks. Instead:

```text
Tokio DBus/plugin/test task
        |
        | calloop channel / std mpsc / oneshot
        v
calloop compositor thread owns &mut Manifold
        |
        | mutates Smithay, sessions, layout, focus, children, etc.
        v
Wayland clients / presentation / control replies
```

This actor boundary is strongest in `control.rs`: each DBus method constructs a `DbusStateRequest`, sends it through a calloop channel, and awaits a one-shot response. The calloop callback executes `execute_state_request()` with `&mut Manifold`.

## 4. Startup flow

The main `run` flow is:

1. `run_cli()` dispatches to `compositor::run(RunArgs)`.
2. `compositor::run()` asks `session_runner::run_if_configured()` whether a project runner should wrap this launch.
   - If a runner is selected, the runner path starts another process under `systemd-run --user --scope`; the original command returns after the runner exits.
   - If no runner applies, it proceeds directly to `serve_session(SessionArgs)`.
3. `serve_session()` starts Tokio and a calloop compositor thread.
4. `serve_session_on_calloop_thread()` performs in-process runtime setup:
   - creates `EventLoop` and Wayland `Display`,
   - constructs `Manifold::new()`,
   - starts the built-in plugin runtime through `state.set_async_runtime()`,
   - applies test flags and optional clearing of the initial surface session,
   - loads runtime settings from config,
   - loads default configured WASM modules unless disabled,
   - loads CLI WASM plugin, key-policy, and test-harness artifacts,
   - records nested-test inner DBus target fields,
   - starts the DBus control service with `control::listen()`,
   - writes Manifold session/control metadata into state,
   - prints `WAYLAND_DISPLAY` and DBus environment values to stdout,
   - attaches a Winit or offscreen presentation unless headless,
   - starts configured startup apps,
   - spawns a CLI command or browser pane if requested,
   - enters `event_loop.run()`.

The event-loop idle callback services the WASM test harness and schedules deferred presentation attaches when `state.attach_requested` is true and no presentation is started.

## 5. The central `Manifold` state object

`state.rs` defines `pub struct Manifold`, which currently aggregates many subsystems. Important groups of fields are:

### 5.1 Compositor/Wayland fields

- Wayland display identity: `socket_name`, `display_handle`, `loop_handle`, `loop_signal`.
- Smithay globals/states: `compositor_state`, `xdg_shell_state`, `xdg_decoration_state`, `shm_state`, `output_manager_state`, `seat_state`, `data_device_state`, `dmabuf`, `popups`, `seat`.
- The nested output and mapped space: `session_output`, `space: Space<Window>`.

`Manifold::new()` creates the Smithay states, adds keyboard/pointer capabilities to the seat, initializes an output named `manifold-session`, maps it into `space`, creates an auto Wayland listening socket, and registers the Wayland display source in calloop.

### 5.2 Presentation fields

Presentation state is tracked with:

- `presentation: Attached|Detached`,
- `presentation_started`,
- `attach_requested`, `attach_schedule_pending`, `detach_requested`,
- attach/runtime failure counters and last error,
- `presentation_window_focused`, pending activation token,
- optional `presenter: Rc<PresenterHandle>`.

Presentation attaches are request-driven. Control commands or UI actions set `attach_requested`; calloop later schedules `presentation::attach()`. Detach requests close the Winit/offscreen target through `PresenterRuntime` and reset transient input state.

### 5.3 Layout, focus, panes, and sessions

The state keeps both a native layout model and a resolved presentation layout:

- `layout: LayoutState` is the native split tree.
- `accepted_resolved_layout: ResolvedLayout` is the actual layout used for hit testing and rendering.
- `resolved_layout_source` says whether that resolved layout came from native fallback or a WASM map policy.
- `layout_leaf_selected_panes` maps native leaves to pane ids.
- `native_session_layouts` stores per-surface-session layout snapshots.

Surface and pane identity fields include:

- `next_pane_id`, `window_pane_ids`, `pending_child_panes`,
- `active_surface_session`, `surface_sessions`, `surface_session_names`, `next_surface_session`,
- per-directory state: `surface_session_directories`, `surface_session_directory_defaults`, and `surface_session_working_directories`,
- `window_surface_sessions` mapping top-level surfaces to sessions.

Focus is tracked through `active_toplevel_surface`, hover/pending pointer focus fields, and Smithay keyboard focus on the seat. `reconfigure_focus_stack()` is the central repair/reconciliation method: it remaps windows to layout locations, raises/focuses an active or fallback window, sends xdg configure state, and clears focus if nothing is visible.

### 5.4 Extensions, agents, status, and grants

`ExtensionRegistry` contains:

- registered extensions,
- registered extension windows and active window roles,
- grants for placement, presentation membership, and shortcuts,
- agent records and notifications,
- status publishers and status items.

The DBus interfaces in `control.rs` call into methods such as `register_extension`, `register_extension_window`, `request_window_placement`, `request_presentation_membership`, `register_agent`, `notify_agent`, and `publish_status`. Placement grants can move a window to a surface session, reserve it as the left panel/sidebar, focus it, or make it a presentation member.

### 5.5 Hosted children, browsers, plugins, WASM, and tests

The state owns:

- `hosted_children` and a hosted child event sender,
- `browsers: BrowserManager`,
- optional `wasm_global_key_policy` and `test_harness`, core-module actor tasks
  and registry state, and optional Component Model and Preview 1 actor runtimes,
- optional `plugin_runtime`, plugin counters/status,
- optional in-process per-session DBus bus handles.

Child process launch can be synchronous or async. Once a Tokio runtime is installed, `launch_hosted_child()` uses `spawn_blocking` to start and wait for the child, sending start/exit/failure events back to the calloop thread.

## 6. Wayland compositor flow

### 6.1 Accepting clients

`Manifold::init_wayland_listener()` creates a `ListeningSocketSource::new_auto()` and stores the socket name. The event source inserts new clients into the Smithay display with `ClientState::default()`. A second calloop source wraps the Wayland `Display` and dispatches client requests whenever readable, then flushes clients.

Hosted clients are launched with `WAYLAND_DISPLAY` set to this socket name. They bind the globals created by Smithay state initializers and interact with Manifold as their Wayland compositor.

### 6.2 New surfaces and commits

`impl CompositorHandler for Manifold` installs a pre-commit hook in `new_surface()`. The hook inspects pending buffer assignments and explicit sync state. If a dmabuf is present, it installs either:

- an explicit-sync acquire-point blocker, or
- an implicit dmabuf readiness blocker.

The `commit()` handler then:

1. records commit latency statistics,
2. runs Smithay `on_commit_buffer_handler`,
3. finds the root surface for non-sync subsurfaces and calls `window.on_commit()` when it belongs to a known top-level window,
4. handles xdg initial configure/popups through `handle_xdg_commit()`,
5. asks presentation code to redraw if an attached presentation has visible windows.

### 6.3 New xdg toplevels

`XdgShellHandler::new_toplevel()` is the main window mapping path:

1. Wrap the `ToplevelSurface` in a Smithay `Window`.
2. Extract the top-level `WlSurface`.
3. Call `state.assign_new_toplevel_surface(surface)`.
4. Allocate a compositor-space location.
5. Map the window into `state.space`.
6. Record window mapping and reconfigure focus.
7. Focus the returned surface.

`assign_new_toplevel_surface()` assigns a pane id and surface session. It tries, in order:

- existing pane mapping,
- matching a pending child by pid or inherited `MANIFOLD_PANE_ID`,
- `MANIFOLD_SURFACE_SESSION_ID` in the process tree,
- default surface session.

It then applies extension grants, handles presentation-child surfaces specially,
places new native panes when native fallback is active, and emits pane/layout
state messages to loaded WASM actors. A plugin actor can answer through the
host mailbox with a validated resolved-layout command.

### 6.4 Fullscreen, decorations, popups, seat, and data device

The current compositor handler supports:

- xdg fullscreen/unfullscreen requests by recording `fullscreen_toplevels`, focusing, and reconfiguring layout,
- server-side decoration mode for all xdg decorations,
- popup tracking/configure through `PopupManager`,
- Wayland seat keyboard/pointer focus and data device focus updates,
- data device/DnD trait plumbing with empty grab handlers,
- output handler delegation.

## 7. Presentation renderer and event source

Presentation is separated into two layers:

- `presentation.rs` ties a `PresenterHandle` to `Manifold`, handles attach/detach, redraw scheduling, rendering, dmabuf advertisement, and frame callbacks.
- `winit_presenter.rs` owns Winit/EGL/GLES specifics and produces `PresenterEvent` values.

### 7.1 Attach targets

There are two presentation targets:

- **Winit**: a visible OS window with a Winit event loop and EGL window surface.
- **Offscreen**: an EGL offscreen renderbuffer, used by tests and headless-style verification.

`presentation::attach()` and `attach_offscreen()` call a shared `attach_target()`. It creates a `PresenterRuntime`, a Smithay output/damage tracker, a `PresenterHandle`, stores it in state, and registers:

- a presentation control timer, always,
- a `PresenterEventSource` for Winit targets only.

For offscreen targets, the control timer also calls `ensure_offscreen()`, drains pending events, and renders if redraw was requested.

### 7.2 Presenter creation

When `PresenterEvent::Created` arrives, `apply_presenter_created()`:

- stores the presenter runtime as the dmabuf importer,
- sets the output mode to the backend window/offscreen size,
- updates SHM formats in `shm_state` if the fallback feature is enabled,
- attempts legacy EGL `wl_display` binding,
- initializes linux-dmabuf and explicit sync advertisement if not already initialized,
- maps the presenter output into `state.space`,
- marks presentation attached,
- clears attach request/error state,
- records attach latency/count,
- requests a redraw,
- applies a pending activation token if one exists.

### 7.3 Rendering flow

`render_presenter_frame_with_capture()` is the main compositor-to-presentation render path:

1. Borrow the presenter backend.
2. Build a temporary `Space<MarkedWindow>`.
3. Map all windows currently visible in the presentation to their layout rectangles.
4. Map the active presentation window again to make it appear on top.
5. Build chrome render elements from `ResolvedLayout` placeholders/handles/chrome/overlays and borders.
6. Bind the GLES renderer to the current Winit EGL surface or offscreen renderbuffer.
7. Call Smithay `render_output` with the temporary space and chrome elements.
8. Optionally copy the framebuffer to a PNG path for debug dumps.
9. Submit/swap buffers for Winit windows; offscreen submit is a no-op.
10. Record frame timing statistics.
11. Send frame callbacks and dmabuf feedback to visible clients unless early callbacks were already sent for a deferred input frame.
12. Request another redraw from the backend.

The debug/test surface marker path wraps `Window` as `MarkedWindow`; when `test_surface_markers` is enabled, it draws deterministic marker patterns before each client surface.

### 7.4 Redraw deferral and latency sampling

Presentation uses small deferrals to avoid presenting too early relative to client commits:

- `CLIENT_FRAME_DEFER` is a fixed client-frame delay.
- `INPUT_FRAME_DEFER_MAX`/`MIN` can delay rendering briefly after input when no client commit has arrived yet.

`schedule_deferred_client_frame()` sets `state.presentation_redraw_deferred`, optionally sends early frame callbacks, and installs a calloop timer. When the timer fires, it renders if still attached and records whether the frame was input-deferred or client-deferred.

Runtime statistics track input-to-frame, input-to-commit, commit-to-frame, client-frame-callback-to-commit, attach/detach latency, and frame duration/interval.

### 7.5 Winit/EGL backend

`winit_presenter.rs` handles:

- creating Winit windows on Wayland/X11,
- wrapping raw Wayland/Xlib displays as EGL native displays,
- creating EGL contexts/surfaces,
- creating offscreen EGL displays from GBM render nodes or surfaceless EGL,
- creating GLES renderbuffers for offscreen targets,
- translating Winit keyboard/pointer/scroll/touch events into `PresenterEvent`,
- Wayland xdg-activation token use for presentation window focus.

`PresenterEventSource` is a calloop `EventSource`. It pumps the Winit event loop in `before_sleep()` and `process_events()`, forwarding pending events into the calloop callback. This keeps presentation events in the same actor thread that owns `Manifold`.

## 8. Input control flow

Input usually enters through `PresenterEvent` values handled in `presentation.rs`, though `Manifold::process_input_event()` also exists for generic Smithay input backend events. Presentation event handlers call methods in `input.rs` and flush Wayland clients afterward.

### 8.1 Keyboard path

`forward_keyboard_key()` maintains modifier state and decides whether Manifold
consumes or forwards each key. Ctrl, Alt, and Shift presses are initially
compositor-owned. Releases are forwarded only if a later key caused the held
modifier to be synthesized to the client.

Every non-modifier event is first offered to the installed
`WasmGlobalKeyPolicy`. The provider receives the key, state, modifier mask,
input mode, and the latest actor publication frame. It returns a bounded
decision envelope plus actor messages. The host applies the decision, enqueues
the messages, and drains the host actor mailbox. If the provider abstains or no
provider is installed, the key is forwarded to the focused Wayland client;
held modifiers are forwarded first when shortcut passthrough is active or the
input mode is not normal.

The checked-in default-workspace plugin supplies the current shortcut and mode
policy. These behaviors are therefore plugin policy, not hard-coded branches in
`input.rs`.

### 8.2 Pointer motion/button/axis path

Pointer motion:

1. If left-panel resizing is active, pointer x adjusts the panel width, reconfigures focus stack, and consumes the event.
2. Otherwise hit-testing determines hover/top-level and nested surface target.
3. The event updates input stats and target hit/miss counters.
4. Smithay pointer motion is sent to the surface under the layout position.

Button handling:

1. Left button press over the left-panel splitter starts panel resize and consumes the button.
2. Left button release after resize ends resizing and consumes the button.
3. Otherwise, on press, Manifold stores the target surface as pending focus if the pointer is not grabbed.
4. On release, it focuses the stored or current target surface. If no surface is targeted and no grab is active, it clears keyboard focus.
5. The button event is forwarded through Smithay pointer input.

Axis handling computes the current pointer target, records stats, and forwards the axis frame.

### 8.3 Hit-testing data flow

Pointer hit testing uses the current presentation layout rather than raw Smithay stack order alone:

1. Panels are checked first.
2. A focused fullscreen window is checked next.
3. The content rect, adjusted for the left panel reserved width, is converted into normalized `LocalPoint` coordinates.
4. `ResolvedLayout::hit_stack()` returns topmost hit items by `z`.
5. Pane items are mapped back to `WlSurface` through pane ids and `window_for_toplevel_surface()`.
6. Placeholders/handles/chrome/overlays stop hit propagation without selecting a client surface.

## 9. Layout architecture

There are two layout representations.

### 9.1 Native layout tree

`LayoutState` is a binary split tree of `LayoutNode` values:

- leaves are `LayoutLeaf { id, kind: Client }`,
- splits are horizontal or vertical with two child nodes,
- the tree tracks `active_leaf` and `next_leaf`,
- resolving the tree produces pixel `LayoutRegion` rectangles for a given `LayoutSize`.

Actions in `LayoutAction` can focus/cycle panes, split active panes, join/remove splits, toggle managed fullscreen, toggle docks, and adjust left dock width. Several dock movement actions currently log that they are accepted but not implemented by the hardcoded layout.

### 9.2 Resolved layout

`ResolvedLayout` is the normalized layout actually used for presentation hit-testing and rendering. It contains `ResolvedLayoutItem` records with:

- nonzero `LayoutItemId`,
- `LayoutItemKind` (`Pane`, `Placeholder`, `Handle`, `Chrome`, `Overlay`),
- finite `z`,
- normalized `LocalTransform` within `[-1, 1]`,
- `InputBehavior` (`None`, `Hit`, `Drag`, `Pane`).

`validate_for_panes()` rejects duplicate ids, duplicate pane references, non-finite geometry, out-of-bounds transforms, unknown panes, and invalid input behavior combinations.

The current state can accept resolved layouts from two sources:

- **Native fallback**: generated from `LayoutState` and `layout_leaf_selected_panes`.
- **WASM policy**: supplied by a validated `set-resolved-layout` message from a
  plugin actor after the host publishes pane/layout state.

When windows close or inventory changes, `repair_resolved_layout_for_current_inventory()` either rebuilds native layout or prunes/revalidates the accepted WASM layout. If a WASM layout becomes invalid, Manifold falls back to native layout.

### 9.3 Native pane placement

When a new toplevel appears in the active surface session and native fallback is active:

- if the active leaf has no live selected pane, the new pane becomes selected there;
- otherwise, Manifold may split the active leaf if the visible pane count is below `native_auto_split_visible_limit`;
- the split axis is chosen by the active rectangle aspect ratio;
- if no split occurs, the new pane may remain hidden until selected.

The auto-split visible limit defaults to `3` and can be changed by config.

## 10. Surface sessions and pane identity

A **surface session** is an in-process workspace-like grouping of windows. Each top-level surface belongs to one `SurfaceSessionId`. The initial state starts with session `1`, unless `--no-initial-surface-session` causes `clear_initial_surface_session()` during startup.

Current session data includes:

- stable numeric ids,
- generated or user-supplied names,
- active session and last-interacted session order,
- per-session working directory,
- directory-to-session associations and directory defaults,
- saved native layout snapshots per session.

Switching sessions saves the active native layout, changes `active_surface_session`, restores that session's native layout or default layout, records interaction order, rebuilds resolved layout, and reconfigures focus.

Closing a session sends close requests to its windows, removes it from all registries, removes directory defaults for it, picks a replacement active session if needed, restores layout, rebuilds resolved layout, and reconfigures focus.

Pane ids are separate from sessions. They identify a top-level window/pane for layout and WASM APIs. Managed child processes receive `MANIFOLD_PANE_ID` and `MANIFOLD_SURFACE_SESSION_ID`. When a surface maps, Manifold tries to correlate it with a pending child by pid ancestry or by those environment variables in the process tree.

## 11. Hosted app and browser process flow

### 11.1 Hosted Wayland apps

`spawn_hosted_app_argv_in_session()` is the main hosted app launch path. It:

1. validates the target surface session,
2. allocates a pane id,
3. builds a `Command`,
4. sets `WAYLAND_DISPLAY` to Manifold's internal socket,
5. sets `MANIFOLD_PANE_ID` and `MANIFOLD_SURFACE_SESSION_ID`,
6. sets current directory from the session working directory,
7. calls `apply_control_environment()`.

`apply_control_environment()` propagates Manifold control environment variables. If a control DBus address is known, it starts or reuses a per-session in-process DBus bus and sets hosted clients' `DBUS_SESSION_BUS_ADDRESS` to that session bus. If the session bus cannot start, the child inherits the control bus. If no control address exists, the code warns that the host `DBUS_SESSION_BUS_ADDRESS` is inherited and policy modes are not enforced.

Presentation helper apps are spawned with `spawn_presentation_app_argv()`. They use the same Wayland socket but set `manifold_session=presentation`; when their toplevel maps, Manifold detects them as presentation children and excludes them from the normal pane layout.

### 11.2 Browser manager

`BrowserManager` supervises isolated Chromium instances. It resolves a Chromium executable from explicit options, `MANIFOLD_CHROMIUM`, `CHROMIUM`, or common binary names. For each browser it creates or uses a profile dir, reserves a localhost DevTools port, applies platform/environment/extension options, launches Chromium, and stores a summary plus child handle.

Control commands under `browser` can open/list/inspect/close browsers and make simple HTTP requests to the DevTools JSON endpoints. Browser instances are reaped opportunistically and killed on manager drop.

## 12. Control plane architecture

### 12.1 DBus service layout

`control::listen()` registers a calloop channel for `DbusStateRequest` and starts a zbus service on the Tokio runtime. The service owns the requested destination name on either a supplied bus address or the user's session bus.

The service currently exposes multiple interfaces/object paths:

- `/org/manifold/Session` with `org.manifold.Session.Control` for token commands,
- `/org/manifold/Session` with `org.manifold.Session` for session methods,
- `/org/manifold/Manager` with `org.manifold.Manager`,
- `/org/manifold/Extension` with `org.manifold.Extension`,
- `/org/manifold/Plugin` with `org.manifold.Plugin`,
- `/org/manifold/Agent` with `org.manifold.Agent`,
- `/org/manifold/Status` with `org.manifold.Status`,
- `/org/manifold/Compositor` with `org.manifold.Compositor`.

Every async DBus method calls `request_state()`. That function creates a Tokio oneshot, sends a `DbusStateRequest` through calloop, waits with a 15 second timeout, and maps errors into DBus failures. The calloop receiver calls `execute_state_request()`, which mutates `Manifold` and responds.

This control flow is central:

```text
DBus client
  -> zbus async interface method on Tokio
  -> DbusStateRequest + oneshot response
  -> calloop channel
  -> execute_state_request(&mut Manifold)
  -> send_response(...)
  -> DBus method return
```

### 12.2 Token command interpreter

The legacy/general-purpose control interface accepts an argv vector plus a `json` flag. `execute_dbus_request()` decodes JSON if requested and calls `execute_tokens()`.

`execute_tokens()` implements commands for:

- attach/detach/focus presentation,
- status and status JSON,
- quit,
- spawning apps,
- hot-plugging WASM modules,
- focus/layout/passthrough actions,
- surface-session list/create/switch/directory association,
- browser subcommands,
- test harness signals,
- nested-inner command proxying,
- debug dumps,
- synthetic input injection.

After token commands, state changes flush Wayland clients so xdg close/configure/input effects reach hosted clients promptly.

### 12.3 CLI control helpers

Most CLI commands (`enter`, `show`, `hide`, `list`, `switch`, `plug`, `ctl`) are clients of the DBus control service.

`open_cli_control_context()` resolves the current directory, discovers a project runner, resolves the control target from explicit args/environment/session marker/default, and may autostart a global server. Autostart only applies to the default DBus destination with no explicit address. It runs the current executable as a headless server with no initial surface session, logs to the cache directory, and waits for `status-json` to succeed.

Important CLI flows:

- **`enter`** creates or reuses a directory-associated surface session, then runs a shell/command with Manifold env vars. Unless `no_capture` is set, it also sets `WAYLAND_DISPLAY` to Manifold's socket so GUI apps launched from the shell become hosted clients. If a project runner is selected, the entered command may itself run through that runner.
- **`show`** resolves a session from explicit id, directory default, entered-session fallback, or creation; switches to it; then attaches/focuses presentation or just focuses an attached presentation.
- **`hide`** resolves/switches a session and detaches if attached.
- **`list`** prints either all surface sessions or sessions associated with the current directory.
- **`switch`** lists sessions, uses direct id or a skim-compatible picker, can create a named session from a query, remembers it for the current directory, and attaches/focuses presentation.
- **`ctl`** can use typed proxy helpers for extension/plugin/agent convenience commands; otherwise it sends raw token commands.

### 12.4 Status snapshots

`StatusSnapshot::from_state()` refreshes window state and hosted children, then samples presentation, window, session, focus, input, layout, browser, agent, plugin, child, frame, dmabuf, and explicit-sync information. `status` formats a large shell-token-style line; `status-json` serializes the same snapshot to JSON.

## 13. Session-local DBus bus

`session_bus.rs` implements a small in-process DBus broker. `Manifold::ensure_session_dbus_bus()` starts one bus per `SessionBusRealm::Surface(session_id)` and stores its `SessionBusHandle`. Hosted clients receive this bus as `DBUS_SESSION_BUS_ADDRESS`.

The bus:

- binds a Unix socket under a temp dir,
- implements enough DBus authentication for local clients (`EXTERNAL`/`ANONYMOUS`, no Unix FD passing),
- assigns unique names like `:1.N`,
- tracks requested well-known names,
- handles core methods such as `Hello`, `RequestName`, `ReleaseName`, `NameHasOwner`, `GetNameOwner`, `StartServiceByName`, `ListNames`, `AddMatch`, `RemoveMatch`, and `GetId`,
- injects sender names into method calls/signals when routing between clients,
- broadcasts signals,
- returns service-unknown errors for unresolved method-call destinations.

If a host bus address is available, the in-process bus presents a virtual owner for `org.freedesktop.Notifications`. Notification methods are proxied to the host notification service. For each proxied notification id, a forwarding thread listens for host `ActionInvoked`, `ActivationToken`, and `NotificationClosed` signals and sends matching signals back to the original session client.

This is currently a focused, hand-written DBus router, not a full DBus broker.

## 14. Config and project session runner flow

### 14.1 Config loading

`config::global_config_path()` selects config from:

1. `MANIFOLD_CONFIG`, if set,
2. XDG config path,
3. a repository `manifold.toml` found upward from the current executable.

The TOML model supports runtime layout settings, module definitions, startup apps, and a session runner configuration. Current code uses it for:

- `layout.auto_split_visible_limit`,
- modules of kind `plugin` or `wasm-plugin` (the old `fast-key-handler` and
  `fast-map-hook` kinds are recognized only to emit a legacy warning and skip),
- startup apps in `session` or `presentation` realm,
- session runner detection.

Startup app argv entries that look like relative paths are resolved against the config directory if the path exists.

### 14.2 Runner selection

`discover_session_runner()` walks upward from a start directory. A `manifold.toml` `[session.runner]` entry has priority. If the manifest runner is disabled, it blocks fallback detection. Otherwise, a `flake.nix` marker selects the built-in `nix-develop` runner.

`session_runner::run_if_configured()` skips runner use when explicitly disabled or when `MANIFOLD_SESSION_RUNNER_ACTIVE` is already set. If a runner applies, it:

1. creates a per-launch cache directory,
2. builds a `LaunchRecord` with session args and inner binary path,
3. writes the record under `XDG_RUNTIME_DIR/manifold/launches`,
4. starts `systemd-run --user --scope --collect --unit manifold-launch-<id>.scope`,
5. runs the runner activation program in that scope,
6. removes the launch record and cache dir when done.

The systemd scope name embeds the launch id, allowing `launch_context::current_launch_record()` to recover the launch record from `/proc/self/cgroup` inside `manifold-sessiond` or `manifold-runner-exec`.

### 14.3 Nix runner sidecar

`manifold-runner-nix-develop` receives project dir, working dir, session cache dir, runner cache root, and inner binary. It caches an activation script generated by `nix print-dev-env --profile`, using a manifest keyed by project path, `flake.nix`, and `flake.lock` checksums. A directory lock prevents concurrent rebuilds. It sources the cached activation script and runs the inner binary with `MANIFOLD_SESSION_PROJECT_DIR`, `MANIFOLD_SESSION_WORKING_DIR`, and `MANIFOLD_SESSION_CACHE_DIR` set.

`manifold-runner-exec` is the sidecar for running an arbitrary entered command through the same runner environment. It reads the launch record and `exec`s `program_argv` in the recorded working directory with launch env pairs.

## 15. WASM extension points

### 15.1 Core-module actors and global key policy

`crates/manifold-wasm/src/module.rs` discovers named actor and policy-provider
exports in a self-contained core module. `WasmActor` runs one bounded actor
epoch at a time against a registered mailbox and dispatches its outbox through
the host registry.

`crates/manifold-wasm/src/global_key_policy.rs` invokes the selected synchronous
key provider with a serialized key query and an optional immutable publication
frame. The provider returns a bounded decision envelope and message batch; the
daemon applies the decision and enqueues messages to named actors. The checked-in
default-workspace plugin combines this key provider with a workspace actor.
See the source files that define module export discovery and policy response
decoding for the exact ABI.

### 15.2 Actor-driven layout policy

Layout is no longer a standalone direct map-hook import lane. When pane
inventory changes, the daemon emits `pane-mapped` and `layout-state` messages
to the loaded plugin actors. An actor can post a `set-resolved-layout` command
to the host actor mailbox. The daemon validates pane identity, geometry, focus,
and input behavior before accepting the layout, and falls back to native layout
when validation fails.

### 15.3 Test harness ABI

`wasm_test.rs` starts a harness in a blocking task. The guest exports `run() -> i32`; exit code `0` means passed, `2` means paused, anything else means failed.

Imports include:

- `sleep_ms`,
- `log`,
- `wait_for_signal`,
- `control_json`,
- `status_json`.

Harness requests are sent over std mpsc to the calloop thread. `Manifold::service_test_harness()` drains requests, serves control/status calls, and reacts to completion by stopping the loop for pass/fail or keeping it paused for inspection.

### 15.4 Component actor prototype

When plugin replacement cannot parse an artifact as a core module,
`Manifold::replace_wasm_plugin()` attempts to load it as a Component Model
actor. `ComponentActor` owns one component instance and Store, a bounded normal
mailbox, a reserved completion queue, actor-scoped worker tasks, an epoch
ticker, and the completion-I/O supervisor. The runnable component imports only
the Manifold actor/worker WIT in
`crates/manifold-wasm/wit/manifold-plugin.wit`; task identity gates worker-only
async calls, and dispatch has no standard blocking-looking WASI surface.

The dispatcher submits reads and writes and later receives typed completion
events. Async workers execute exports from the same component instance and may
await worker I/O or an explicit cooperative `worker-yield`, releasing the Store
so dispatcher traffic can proceed. Epoch deadlines are correlated to the
dispatcher guest task, so they do not trap an unrelated worker. Configured
actors can receive read-only or writable access to the active session working
directory through explicit workspace filesystem capabilities. Actors without
one retain read-only access to the artifact's parent directory.

The same ABI exposes generation-scoped notifications and notification
pollables. Actor watches are one-shot and coalesced; workers use bounded
`worker-wait-any` with biased or rotating-fair selection and implicit worker
cancellation. The guest owns its shared-memory queue or synchronization layout.
The host reports live resources and wait registrations but never implements a
guest mutex or channel.

Individual worker cancellation is cooperative and becomes terminal only after
the guest export returns. Replacement calls `ComponentActor::shutdown()`,
terminates the owning Store, and joins the old generation's runtime, I/O
supervisor, tracked blocking I/O, and epoch task before activating a new
generation. `tools/test-wasm-component-worker.sh` exercises this runtime
directly.

The daemon stores Component and Preview 1 instances together in a deterministic
hosted-I/O actor map keyed by stable instance ID. Configuration uses the
`[module.<id>]` ID, while `manifold plug --id <id>` exposes the same operation at
runtime. Replacement removes and joins only that entry. Routing, actor lists,
mailbox counters, broadcast delivery, and shutdown iterate the full map rather
than a singular compatibility slot. This instance-ID contract applies to the
Component and Preview 1 hosted backends. Legacy core-module inventories keep
their declared actor names and singular replacement lane, so the loader rejects
a custom runtime instance ID for them instead of pretending that ID was
registered. Their configuration table key remains a module label for backward
compatibility with the checked-in workspace policy.

### 15.5 Preview 1 compatibility actor and workers

A core module exporting the Preview 1 compatibility marker is routed to
`Preview1CompatActor`. The module imports one shared memory. Its actor
dispatcher and each worker use separate Wasmtime Stores and instances over that
memory; this preserves shared-memory communication while keeping descriptor
tables and worker-local WASI state private to each Store.

The actor Store has no blocking filesystem or stdin resources and uses the
completion imports. Worker Stores derive standard Preview 1 WASI filesystem and
stream authority from the actor grant, so ordinary calls such as Rust
`std::io::Read::read_exact` suspend only the worker's Tokio task. Worker
stack/TLS reservations are allocated and released through the guest
compatibility ABI. CPU-running Preview 1 workers yield through epoch async
scheduling because they have independent Stores.

The compatibility ABI shares the same notification/pollable hub across the
actor and every worker Store. The example's primary actor/worker path is a
guest-owned atomic queue; host notifications suspend workers and schedule a
coalesced actor event. The host-owned stdin duplex is retained only for an
explicit ordinary-WASI compatibility case.

Capability-rooted filesystem watches use the daemon's shared native watcher
service through `filesystem_watch.rs`. Canonical path and recursion-mode
registrations are deduplicated while each logical watch owns a private bounded
queue and one generation notification shared across
the actor and worker Stores. Workers therefore select watch readiness through
the existing bounded `wait-any` path, then drain relative-path event batches;
recursive backend setup runs as host-owned work and publishes an initial rescan
marker once registration finishes. Overflow uses the same explicit marker.
Paths remain lossless native bytes in the shared queue and Component ABI;
Preview 1 compatibility batches use reversible `%HH` escaping for non-graphic
bytes and the escape introducer.
Watchers and their native threads are dropped before the actor generation's
coordination hub stops.

The compatibility facade appears in the normal plugin actor list, accepts
messages through the existing DBus/control actor surface, and reports mailbox,
completion, notification, pollable, active-wait, worker, generation, and
last-result counters in status output. Replacement cancels pending workers and
waits for their Stores, wait-registration teardown, and the completion
supervisor before the new actor becomes reachable.

`tools/test-wasm-preview1-worker.sh` exercises the low-level runtime and actor
facade in-process. `tools/test-wasm-preview1-worker-nested.sh` loads the artifact
through the daemon's built-in nested test topology and proves control-surface
routing, actor progress during notification select, completion I/O, ordinary
WASI read compatibility, and hot replacement while two notification wait sets
are pending.

`tools/test-wasm-io-registry-nested.sh` loads four instances of each backend,
holds workers pending in one generation, replaces only that actor, and verifies
the other seven continue accepting independently accounted mailbox traffic.

## 16. Plugin runtime

`plugin_runtime.rs` is separate from WASM. `Manifold::set_async_runtime()` starts
`PluginRuntime::start_builtin()`, which currently creates a native host mailbox
and one built-in actor:

- id `debug-toggle`,
- kind `builtin-toggle-launcher`,
- one bounded mailbox slot named `default`,
- one publication slot named `default`.

The actor toggles a `spawn_btop_next` boolean each time it receives a mailbox
message and publishes a `ToggleChoice` frame back to the host through a calloop
channel. The host appends frames into the actor's publication state. No input
path currently consults this frame; synchronous key decisions are handled by
the WASM global-key policy described above.

The DBus plugin interface can list actors/mailboxes and enqueue actor messages. Status snapshots expose plugin actor summaries and counters.

## 17. Extension, agent, status, and sidebar app flow

The typed DBus extension/compositor/status/agent APIs build on the `ExtensionRegistry` in state.

Typical presentation sidebar flow, as implemented by `apps/manifold-session-switcher.py`, is:

1. Connect to Manifold DBus using `MANIFOLDD_DBUS_ADDRESS`/`MANIFOLDD_DBUS_DESTINATION` or defaults.
2. Register an extension with role `session-switcher`.
3. Register a compositor window identity, usually by pid.
4. Replace the active presentation session-switcher role so old sidebars can be dismissed.
5. Request presentation membership.
6. Request placement as a left sidebar/panel with a width.
7. Optionally request a shortcut grant.

On the Manifold side, those DBus calls create extension/window/grant records. If the matching toplevel surface is already mapped, placement and membership apply immediately; otherwise `apply_registered_extension_grants_for_surface()` applies active grants when the window later maps.

The sidebar also uses:

- session list/activation/create APIs,
- control commands for focus-window and attach-focus,
- agent list/clear/focus behavior,
- status list/signals for passthrough mode,
- desktop notifications and activation tokens.

Agent records are stable by identity, preferring `pane_id`, `pidfd`, or `pid` metadata. Notifications set agent attention/unread counters and may update status for waiting/blocked/error levels. Status items are keyed by channel and can be published by Manifold itself or by registered status publishers.

## 18. Debug and test architecture

### 18.1 Debug dump

`debug_dump::dump_bundle()` writes a manifest plus selected artifacts:

- `state.json`: filtered status, windows, layout items, active rect, marker ids,
- `composed.png`: a rendered presentation frame captured through `presentation::dump_composed_frame()`,
- `surfaces/*.png`: current SHM buffers for visible toplevels where the buffer format is supported.

Surface capture currently reads SHM buffers through Smithay helpers and converts ARGB/XRGB to RGBA. Non-SHM or unsupported buffers are recorded in the manifest as unsupported rather than captured.

### 18.2 Nested test topology

`compositor::run_test()` defaults to `nested`. The nested workflow can run in two topologies:

- **Outer supervisor**: starts an outer Manifold process, optionally offscreen; spawns an inner Manifold inside the outer by sending an outer control `spawn` command; waits for both control services; optionally waits for the inner presentation to map as an outer window.
- **Current session**: if already inside a Manifold session and not forced outer, starts only the subject/inner process in the current environment.

The test helper creates temporary logs/artifacts, uses unique DBus destinations, polls `status-json`, can send an inner command, can capture an outer debug dump, and cleans up via `quit` unless told to keep running or waiting for a harness.

## 19. Important end-to-end data/control flows

### 19.1 Managed app launch to first frame

```text
CLI/control/startup requests spawn
  -> Manifold::spawn_hosted_app_argv_in_session
  -> allocate pane id, set WAYLAND_DISPLAY/MANIFOLD_PANE_ID/MANIFOLD_SURFACE_SESSION_ID
  -> optionally start session-local DBus bus and set DBUS_SESSION_BUS_ADDRESS
  -> child connects to Manifold Wayland socket
  -> Smithay xdg new_toplevel callback
  -> assign pane/session, apply grants, native placement or actor layout event
  -> map Window in Space
  -> focus/reconfigure
  -> client commits buffer
  -> compositor commit records stats and requests redraw
  -> presentation renders Space<MarkedWindow> + chrome
  -> swap/capture, send frame callbacks/dmabuf feedback
```

### 19.2 Presentation input to hosted client

```text
OS/Winit event
  -> PresenterEventSource pumps Winit
  -> presentation::handle_presenter_event
  -> state.forward_keyboard_key / pointer method
  -> Manifold shortcut/plugin/WASM/layout handling maybe consumes
  -> otherwise Smithay seat forwards to focused/target WlSurface
  -> display_handle.flush_clients()
  -> deferred or immediate presenter redraw requested
```

### 19.3 DBus control mutation

```text
CLI or external DBus client
  -> zbus proxy/interface call
  -> async DBus method creates DbusStateRequest
  -> calloop channel to compositor thread
  -> execute_state_request or execute_tokens mutates Manifold
  -> optional Wayland flush
  -> oneshot response to zbus method
  -> CLI prints response or external client receives typed value/signal
```

### 19.4 Directory session environment (`enter`)

```text
manifold enter
  -> resolve/autostart DBus control target
  -> resolve or create surface session for current directory
  -> remember directory default if needed
  -> get status-json for WAYLAND_DISPLAY
  -> run shell/command with manifold_session, DBus env, surface-session id, and maybe WAYLAND_DISPLAY
  -> GUI programs launched from that shell connect back to this Manifold session
```

### 19.5 Project runner launch

```text
manifold run
  -> discover session runner from manifold.toml or flake.nix
  -> create launch record under XDG_RUNTIME_DIR
  -> systemd-run --user --scope manifold-launch-<id>.scope runner-sidecar ... manifold-sessiond
  -> sidecar activates project env (for nix, cached print-dev-env)
  -> manifold-sessiond reads launch record from current cgroup
  -> serve_session(SessionArgs from record)
```

## 20. Current implementation boundaries and notable constraints

These are not future plans; they are observable properties of the current code:

- Runtime state is highly centralized in `Manifold`. Geometry, sessions, focus, DBus registries, process supervision, plugins, and stats are all in one struct.
- The compositor thread is the single mutation point for `Manifold`; async DBus/plugin/test tasks must use channels.
- The control command interpreter is token-oriented and manually parsed. Typed DBus interfaces coexist with the token command path.
- Native layout is a binary split tree of client leaves only. Docks are represented mostly by booleans and the special left-panel reservation path; several dock movement actions are placeholders.
- The accepted presentation layout may be native-generated or supplied through
  a WASM actor's host message. Plugin layouts are accepted only after validation
  but are still repaired or replaced by fallback when window inventory changes.
- Presentation can be detached while the Wayland compositor continues running. Hosted clients can still be present; visible latency stats only sample when presentation is attached and relevant windows/targets exist.
- The in-process DBus bus is intentionally limited. It implements enough for session-local name ownership/routing and notifications, but not full broker features such as Unix FD passing.
- Browser support is process/DevTools based and Chromium-specific.
- Offscreen presentation uses EGL/GBM/surfaceless paths and is primarily wired for tests and automation.
- The checked-in GTK sidebar is an external DBus client, not part of the Rust compositor state; it influences presentation through extension/window/placement grants.
