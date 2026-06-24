# WASM Integration

This is the canonical WASM design-target document.

It describes the intended Manifold app and module model, not a promise that every interface or command described here exists today. Mention current implementation details only when they affect usability.

WASM is Manifold's planned fast-iteration layer for workspace behavior, built-in tools, user apps, extensions, and agents. Manifold should have a workspace and WASM app model; the open question is the exact shape, not whether this direction belongs in the project.

The integration should make behavior reloadable without rebuilding compositor internals, while preserving Manifold's core rule: authoritative workspace, session, and app state live behind the presentation surface, and presentation endpoints render the latest scene and surface state for one session at a time. More native, browser, and Wayland experiments are needed before freezing the API, ABI, runtime crate layout, or WIT package shape.

## Goals

- Control workspace behavior through reloadable modules.
- Let WASM pane apps create panes that render in presentation surfaces while their state remains in the session process.
- Provide capability-scoped APIs for WASM apps, controllers, extensions, and agents to interact with each other, with pane producers, with managed apps, and with the host.
- Let one WASM app create any number of panes within its session.
- Treat session membership as the first-phase trust boundary, so WASM apps in the same session can communicate through session APIs and granted capabilities.
- Make Linux-native development the first fast-turnaround path.

## Non-Goals For The First WASM Pass

- Do not expose Smithay, Weston, EGL, DMA-BUF, or renderer internals to WASM modules.
- Do not implement browser, terminal, PTY, or raw GPU access inside arbitrary WASM modules.
- Do not grant unrestricted filesystem, process, network, editor, or shell access.
- Do not treat same-session trust as internet-safe multi-user isolation.
- Do not require a virtual filesystem for the first workspace/WASM pass.
- Do not split out a separate presentation endpoint just to land the first WASM host.

## Architectural Rule

WASM modules talk to Manifold through a stable workspace/app API, not through compositor internals.

This means the first useful milestone is not the WASM runtime itself. The first milestone is a native session model, with any needed workspace-level model around it, that can represent sessions, panes, pane producers, focus, commands, events, scene state, surface slots, and presentation requests independently from the current Smithay `Space<Window>` representation. Presentation layout, dragging, and animation belong to the presentation surface, not the producer model.

The runtime then becomes an adapter over this model. Linux-native `manifoldd` can host the runtime in-process at first, while later backend work can satisfy the same behavior through protocol-facing session processes or backend-specific adapters if needed.

The execution model is recorded in [WASM Execution Model](wasm-execution-model.md). In short, Manifold should keep calloop as the authoritative session/compositor loop, run ordinary plugin work in a separate async lane, and reserve a synchronous fast lane for bounded pre-commit policy hooks.

The layout policy boundary is recorded in [Layout Mould](layout-mould.md). In short, WASM may produce or transform logical layout data, while the native host renders, hit-tests, validates constraints, and owns fallback behavior.

The realm binding and lifetime model is recorded in [WASM Realms](wasm-realms.md). In short, modules should target `global`, `session`, or `surface` realms, with mostly shared host APIs plus realm-local convenience bindings and lifetimes.

The native UI backend experiment is recorded in [Session Switcher UI Experiment](session-switcher-ui-experiment.md). In short, WASM should write a flat host-validated UI tree, while native backends such as Iced or GPUI render the accepted tree without becoming the plugin ABI.

The entered session environment model is recorded in [Session Environments](session-environments.md). In short, `manifold enter` should bind shells, commands, GUI apps, agents, and optional filesystem/network/DBus isolation layers to one session context.

This boundary is recorded in [ADR 0005](decisions/0005-wasm-through-workspace-api.md).

## Module Roles

### WASM Controllers

WASM controllers change workspace behavior through host APIs.

Examples:

- focus rules
- presentation requests
- command behavior
- workspace automation
- agent-driven UI actions

Controllers should receive workspace events and request commands. They should not mutate compositor state directly.

### WASM Pane Apps

WASM pane apps are pane producers. They own durable app state in the session process and may create any number of panes by emitting semantic scene patches.

Examples:

- git status, diff, commit, and tree tools
- settings
- dashboards
- inspectors
- agent panels
- project tools
- file and symbol explorers

The presentation renderer executes the fast draw path from scene commits. WASM pane apps should not emit full-frame pixels as their primary output, and pane state should not be owned by the presentation surface.

### Extensions

Extensions add bounded capabilities to the workspace, such as commands, adapters, inspectors, or app-specific integrations.

Extensions should declare their requested capabilities in a manifest. The host should grant only explicit permissions.

### Agents

Agents can observe events, request actions, and communicate with apps or panes through capability-scoped APIs.

Agents should operate through the same workspace command and event model as human-driven UI, so agent behavior is testable with the same smoke workloads. They are treated like other apps inside the session trust boundary, with the added requirement that requested and performed actions are logged in the session.

## Host Components

### Workspace Core

The workspace core is the durable model that WASM and native code share.

Initial concepts:

- `WorkspaceId`
- `SessionId`
- `PaneId`
- `ProducerId`
- `AppId`
- `SurfaceSlotId`
- `WorkspaceCommand`
- `WorkspaceEvent`
- `ScenePatch`
- `PresentationRequest`
- `PresentationResponse`

The current Smithay compositor path should become an adapter that projects Wayland toplevels and surface commits into this model.

### WASM App Host

The WASM app host loads modules, stores per-app state, grants capabilities, invokes lifecycle functions, and applies requested workspace changes.

Preferred direction:

- Wasmtime Component Model for the runtime.
- WIT for versioned host APIs.
- A small host API surface at first, expanded only when real apps need it.
- A compile/runtime cache for fast reloads.
- Per-app storage rooted in a host-controlled directory.

The exact runtime crate layout remains open until the workspace core exists.

The host should also own realm instantiation:

- one `global` realm contract for the session process lifetime
- one lazy `session` realm contract per session
- one lazy `surface` realm contract per presentation surface

Modules should target one realm kind explicitly. A single module binary may then have many live instances of that realm kind, such as one build-manager instance per session.

### Capability Broker

The capability broker checks every privileged host operation against both:

- the module manifest request
- the user's or session's granted capability set

Capability families should start narrow:

- `workspace:*`
- `scene:*`
- `storage:*`
- `commands:*`
- `files:read`
- `files:write`
- `process:spawn`
- `host:clipboard`
- `agent:*`

The broker should return explainable denial errors so failed experiments are easy to debug.

Realm choice should not be treated as a separate hard security boundary. It primarily changes binding, lifecycle, and API ergonomics. Capability checks still apply at the operation level where Manifold needs them.

### Event Bus

The event bus delivers bounded workspace and app events to controllers, pane apps, extensions, agents, and host adapters.

Examples:

- pane opened, closed, focused, resized, moved
- producer attached, detached, crashed, restarted
- session cwd or environment changed
- scene committed
- command invoked
- managed app title/app-id changed
- presentation attached or detached

Event delivery should have backpressure and diagnostics. Slow modules should be visible in status output and logs.

Event delivery belongs to the async lane. Latency-sensitive decisions that must happen before a workspace transaction commits should use the bounded fast lane described in [WASM Execution Model](wasm-execution-model.md).

Realm-local events should be easy:

- global modules may observe the whole process
- session modules observe their bound session and its child objects
- surface modules observe their bound surface and receive explicit events when the attached session changes

### Scene Store

The scene store holds retained semantic scene state for WASM pane apps and other scene-producing systems.

It should support:

- patch application
- stable element IDs
- hit-test metadata
- surface slots
- invalidation
- presenter-local caching and animation intent

The presentation endpoint renders scene state and imported surfaces. It should not own authoritative app state.

## API Shape

The initial API should be intentionally small:

- `init`
- `update(event) -> list<requested-command>`
- `render() -> scene-patch`

The first concrete command/API spike is documented in [WASM API Prototype](wasm-api-prototype.md). It should prove, in order, workspace navigation, running managed apps, and keyboard shortcut registration/dispatch. Those operations already exist in the native prototype as focus movement, hosted app spawning, presentation attach/detach, and normal-mode key handling, so they are the lowest-risk way to exercise the WASM host boundary.

Candidate WIT packages:

- `manifold:workspace`
- `manifold:scene`
- `manifold:events`
- `manifold:storage`
- `manifold:commands`
- `manifold:host`
- `manifold:agent`

Same-session apps should be able to communicate directly through host-mediated session APIs. Cross-session communication should require an explicit workspace-level grant or bridge.

The API should mostly be one object-addressed host surface with realm-local conveniences layered on top of it.

The intended shape is:

- global APIs operate on explicit ids such as `SessionId`, `SurfaceId`, and `PaneId`
- session-realm modules get `this_session()` so they do not need to pass their own session id
- surface-realm modules get `this_surface()` and may also get a fallible `attached_session()`

Outside the bound realm object, references should be weak. Object lookups and cross-realm operations should therefore return `result` or `option` when the target may have disappeared.

Panes are ordinary objects, not a separate realm kind. Modules should be free to create any number of panes when their host capabilities allow it.

The Rust side can first model these as native traits and structs. Once the semantics are proven, the WIT interface should mirror the same boundary.

## Realms

The current target design is three realm kinds:

- `global`
- `session`
- `surface`

These are API and lifetime contracts for module instances, not replacements for the workspace and session object model and not separate trust boundaries.

### Global Realm

Global modules are tied to the session-process lifetime. They are suited to cross-session or background work such as brokers, registries, automation, and system-wide policy.

Global modules should access sessions, surfaces, and panes explicitly by id or lookup. Because the global realm can outlive those objects, all such access must be treated as weak and fallible.

### Session Realm

Session modules are tied to one session and should be the default place for durable per-session behavior such as build managers, project tools, git/status apps, and session-local agents.

Session modules should have implicit access to their own bound session and explicit access everywhere else. One module binary may be instantiated many times across many sessions. By default, session-targeted modules should auto-start for matching sessions and may choose to decline activation during `init`.

### Surface Realm

Surface modules are tied to one presentation surface and should host presentation-local behavior such as arranging panes, manipulating the current view, switching the presented session, and other surface-scoped UI actions.

A surface may switch which session it presents without recreating the surface-realm instance. The instance stays alive and receives an explicit switch event. `this_surface()` remains stable for the life of the instance, while any convenience access to the currently attached session remains fallible.

## Cross-Module Coordination

Manifold should provide minimal built-in cross-module coordination primitives rather than forcing every interaction through a custom global broker.

The initial built-ins should be:

- a registry
- mailbox-style messaging

Instance ids should be the default addressing model. Named aliases or higher-level service routing can be layered on top later through the registry or a broker module.

RPC can remain a later addition if real workloads prove it necessary.

## Data Flow

1. A host adapter observes compositor, browser, terminal, control, or agent activity.
2. The workspace core applies a typed `WorkspaceCommand`.
3. The workspace core emits `WorkspaceEvent` values.
4. WASM controllers and apps receive relevant events based on subscriptions and capabilities.
5. WASM modules request more commands or emit scene patches.
6. The scene store applies patches and records invalidations.
7. The presentation path composites scene primitives and surface slots at refresh rate.

## Hot Reload Workflow

Linux-native development should make reload cheap.

The target usability is a host-managed load, reload, inspect, and unload loop for modules and pane apps. The exact CLI or control-socket surface is intentionally not frozen here.

Current usability note:

- the exact currently implemented CLI entry points are defined in `crates/manifoldd/src/main.rs`
- config-loaded startup modules are defined in `manifold.toml` and parsed in `crates/manifoldd/src/config.rs`
- the current runnable WASM paths are the fast key handler in `crates/manifoldd/src/wasm_key.rs` and the test harness in `crates/manifoldd/src/wasm_test.rs`

Reload policy should be explicit:

- preserve app storage by default
- reset in-memory app instance state unless the module implements a migration hook
- preserve pane identity where possible
- show reload failures in status output without tearing down the session process

When a module binary has many live realm instances, reload should restart all live instances of that module.

## Milestones

### 0. Maintain The Shared Behavior Spec

Keep the shared behavior spec current for pane identity, focus, attach/detach, show/hide, input routing, session persistence, browser testing shape, and window grouping.

This keeps Linux-native WASM work aligned with the product behavior model.

### 1. Add Native Workspace Core

Create a typed workspace model before embedding WASM.

Minimum useful behavior:

- represent panes as presentation units and pane producers as indirect state or content sources behind them
- map hosted Wayland toplevels to pane records
- represent focus commands and actor-like presentation requests/responses
- emit workspace events
- expose JSON control commands for tests

Validation:

- unit tests for command/event behavior
- existing headless smoke tests still pass
- real hosted app workload still maps, focuses, detaches, and reattaches

### 2. Add Scene Store

Introduce retained semantic scene state and surface slots.

Minimum useful behavior:

- a native test producer can emit a simple scene patch
- presentation can render or at least account for scene-backed panes
- Wayland/browser surfaces can be represented as surface slots

Validation:

- smoke test opens a scene-backed pane and a hosted Wayland app together
- status output reports scene panes, surface slots, and patch counters

### 3. Embed WASM Host On Linux Native

Load a small WASM module in `manifoldd` and drive it through the workspace core.

Minimum useful behavior:

- load, reload, list, logs, unload
- `init`, `update`, and `render`
- per-app storage
- capability denial path
- status counters for module calls, failures, and reloads

Validation:

- hot reload a trivial controller without restarting `manifoldd`
- hot reload a trivial WASM pane app and preserve pane identity
- smoke test denial of an ungranted capability

### 4. Add Controller APIs

Let WASM controllers influence real workspace behavior.

Minimum useful behavior:

- subscribe to pane/focus/presentation events
- request focus changes
- request pane creation or movement
- register commands

Validation:

- run a controller that changes initial pane placement or focus policy
- verify behavior through real input and status metrics

### 5. Add App And Agent APIs

Let WASM pane apps, extensions, and agents communicate through host-mediated APIs.

Minimum useful behavior:

- app-to-app messages by capability
- command registry
- agent event subscriptions
- agent action logging
- bounded host operations

Validation:

- an agent panel observes workspace events
- a git/status-style app updates scene state without restarting the session

## Open Questions

- Which scene primitives are sufficient for the first useful WASM pane app?
- Should WIT be introduced at the same time as Wasmtime, or after the native Rust API proves the shape?
- How much app state should be migratable across hot reload?
- How should module CPU time, memory, event backlog, and failures appear in `status-json`?
- What is the smallest real app that proves the API: settings, git status, or an agent inspector?
- Where should user-installed WASM modules live on disk?
- How should capability grants be edited and audited?
- Which session isolation profile should agent-facing WASM and command workflows use by default?
