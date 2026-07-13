# Manifold Context

Manifold is a local persistent graphical workspace that explores whether an extra compositor/session layer can feel native

## Language

**Workspace**:
The product-level Manifold environment: the user's virtual workspace as a whole, including sessions, saved presentation preferences, configuration, and shared project context.
_Avoid_: single session, presentation window

**Session**:
An isolated collection of related panes, producer state, environment, cwd, agents, programs, and capabilities that can be presented, configured, and managed together.
_Avoid_: presentation surface, layout

**Session trust boundary**:
The assumption that programs, agents, and WASM apps inside the same session may discover and communicate with each other according to session capabilities.
_Avoid_: cross-session ambient trust, internet-safe sandbox boundary

**Session process**:
The authoritative process that owns durable session state and the wider workspace-level environment around it. In the presentation protocol role it is the session backend.
_Avoid_: presentation client, display server, final renderer

**Pane**:
A unit of presentation in a Manifold workspace whose state lives behind a layer of indirection rather than in the presentation surface itself.
_Avoid_: app, process, native child window, tab

**Pane producer**:
Anything that can create or supply content for one or more panes, including a supervised pane client process, a managed app surface, or a plugin/WASM program.
_Avoid_: treating plugin, actor, process, renderer, or pane as interchangeable

**Pane client**:
A supervised process that produces scene data or surfaces for one or more panes.
_Avoid_: native child window, OS window

**Plugin**:
A capability-scoped extension package that may contain actors, actor-scoped workers, sync policy lanes, pane producers, UI views, and commands.
_Avoid_: pane, process, renderer, compositor plugin

**Plugin actor**:
The host-visible plugin isolation, addressing, authority, and lifecycle domain. An actor owns an address space, mailbox slots, capabilities, resources, publications, and durable local state. It may be dormant while the host waits for an event; once invoked, its completion-driven dispatcher must not suspend or block.
_Avoid_: sync policy lane, thread, direct host callback

**Plugin worker**:
An actor-scoped execution context that shares its actor's memory, resources, and capabilities. Workers may use blocking-looking WASI calls because only the calling worker is suspended. A worker has no public address or mailbox and cannot outlive its actor.
_Avoid_: actor, independent capability boundary, globally addressable service, guaranteed parallel thread

**Mailbox**:
The host-provided durable message path between actors, host services, panes, and other granted targets.
_Avoid_: sync key/layout path, shared mutable memory

**Completion**:
A typed terminal result for an actor-submitted operation or worker wait. Actor completions are delivered through a reserved actor-private mailbox lane; worker completions resume the waiting worker or notify the actor through an actor-local handle.
_Avoid_: readiness notification, blocking actor call, unbounded callback

**Notification**:
A level-triggered, generation-based scheduling signal paired with guest-owned shared memory. Workers may wait or select on it; actors receive a coalesced actor-private event and never wait on the underlying worker synchronization structure.
_Avoid_: semantic mailbox message, mutex, one event per increment

**Pollable**:
A host resource representing readiness or terminal state for a notification, timer, stream, process, worker, watch, socket, or cancellation source. Workers may select across pollables; actors receive corresponding events.
_Avoid_: blocking actor future, guest mutex layout

**Sync policy lane**:
A special plugin-owned synchronous routine that the host may invoke for immediate pre-commit decisions such as key routing or layout. It reads immutable published state and returns a bounded answer plus messages.
_Avoid_: actor, background worker, blocking RPC

**Swapchain**:
A host-owned latest-state publication path. Plugin actors ask the host to append immutable frames; sync policy lanes read the latest valid frame without draining a queue.
_Avoid_: event log, mailbox, shared mutable heap

**WASM app host**:
A runtime in either the session backend or presentation client that loads approved WASM modules, grants placement-appropriate capabilities, and manages actor lifecycle. Presentation hosts own local policy and UI actors; backend hosts own session actors and backend capabilities.
_Avoid_: assuming all WASM runs in the session process, renderer, compositor plugin

**WASM controller**:
A WASM-backed plugin role that changes workspace behavior through host APIs, such as focus rules, presentation requests, commands, sync policy lanes, or automation.
_Avoid_: compositor plugin, direct state mutator

**WASM pane app**:
A WASM application whose presentation actor may create panes through semantic UI or custom render packets and whose durable model may be client-local, backend-coordinated, or owned by a paired session actor.
_Avoid_: managed app, assuming graphics cross the backend connection

**Managed app**:
An external native application whose windows or surfaces Manifold tracks, places, and preserves as part of a session.
_Avoid_: captured window, child window, remote app

**Hosted Wayland app**:
A managed app launched under Manifold's nested Wayland compositor.
_Avoid_: treating every managed app as a nested Wayland app

**Presentation window**:
The normal desktop window opened by a presentation endpoint to show a session on the user's current compositor.
_Avoid_: remote client, framebuffer viewer

**Presentation surface**:
An independent viewport that presents one session at a time, choosing which panes are visible and how they are arranged, animated, and interacted with. It has state separate from the backend and from other viewports.
_Avoid_: session, pane owner, canonical backend layout

**Presentation client**:
The native or web endpoint runtime that owns one or more presentation surfaces, client-side WASM actors, platform integration, a WGPU renderer, and demand-driven window subscriptions.
_Avoid_: Wayland client, session backend, thin framebuffer viewer

**Presentation lease**:
Generation-bearing authority held by at most one presentation client for one session. It authorizes hosted Wayland window configuration and primary application input while allowing other clients to observe.
_Avoid_: ownership of the session, permission to mutate another client's layout

**Frame subscription**:
A bounded request by a presentation client for the current and future committed content of one backend window producer through a negotiated transport.
_Avoid_: pane ownership, implicit application resize, unbounded frame queue

**Presentation request**:
A message from a pane producer to a presentation surface describing desired presentation behavior, such as size, grouping, modality, attention, or placement hints.
_Avoid_: layout command, guaranteed placement

**Native component pane**:
A plugin pane whose content is described as presentation-rendered components instead of plugin-drawn pixels.
_Avoid_: webview requirement, raw framebuffer by default, backend-rendered UI requirement

**Render packet**:
A bounded, versioned, replayable command buffer submitted by a presentation actor as the latest custom-drawn visual state of one pane. It may reference client-host-managed graphics resources, while the presentation client owns validation, WGPU lowering, GPU synchronization, composition, and presentation.
_Avoid_: native GPU command buffer, graphics swapchain, network-rendering requirement, pointer into retained guest memory

**Session DBus proxy**:
A session-scoped DBus bridge for external apps that need normal desktop-style communication inside a Manifold session.
_Avoid_: host-wide DBus access, cross-session DBus

**Session filesystem access**:
The first-phase model where programs and WASM capabilities operate on the host/project filesystem through session cwd, environment, and grants rather than a virtual filesystem.
_Avoid_: mandatory VFS, hidden filesystem sandbox

**Session environment**:
Mutable session state such as cwd and environment variables that programs inherit at launch and session-aware apps may observe or update.
_Avoid_: immutable launch template, global process environment

**Power-user default**:
Manifold's bias toward allowing explicit user/session actions without confirmation prompts by default, while making effects observable and reversible where practical.
_Avoid_: nagging confirmation, safety theater

**Agent action log**:
A session-local durable or inspectable record of actions requested or performed by agents inside that session.
_Avoid_: hidden agent action, untraceable automation

**Presentation endpoint**:
The component role that owns final presentation execution. The target endpoint is a presentation client; use the role term when process or network placement is not the point.
_Avoid_: session backend, Wayland client

**Detach**:
Closing or removing a presentation window while preserving the session and any managed apps that still have live state.
_Avoid_: quit, close app

**Local presentation indirection**:
The extra local layer between managed app content and the user's desktop compositor, intended to preserve app/session state while still feeling like native windows.
_Avoid_: remote desktop, streaming

**Default implementation assumption**:
In durable prose, unqualified references to "the implementation", "the backend", or default behavior mean the Linux-native Manifold path.
_Avoid_: treating experimental backend notes as supported defaults

**Shared behavior spec**:
The product-level Manifold semantics that backend work should try to satisfy, such as pane identity, focus rules, attach/detach behavior, show/hide, session persistence, and window grouping.
_Avoid_: shared runtime, common compositor abstraction

**Capability grant**:
An explicit permission that lets a WASM module use a host API for a bounded purpose.
_Avoid_: ambient permission, unrestricted access

## Relationships

- The current `manifoldd` binary is the MVP **session process** and also owns the MVP **presentation endpoint**.
- The target `manifold-client` is an independent portable **presentation client** without authoritative session state. The split is first introduced as a typed in-process boundary.
- A **presentation client** owns viewport state, client-side actors, graphics resources, WGPU composition, and selected **frame subscriptions**.
- At most one client holds the generation-bearing **presentation lease** for a session; zero clients is a normal detached state.
- A **workspace** is the product-level umbrella that contains one or more **sessions**.
- A **session** contains related panes, producer state, environment, cwd, agents, programs, and capabilities isolated from other sessions.
- The **session trust boundary** is the primary first-phase trust model: things in the same session are assumed to trust each other, while cross-session access requires explicit workspace-level bridging or grants.
- **WASM pane apps** may run primarily in a presentation client. They coordinate explicitly with session actors or backend storage when state should follow the session; otherwise their state remains client-local.
- External apps in a **session** may use a **session DBus proxy** for session-local desktop integration without receiving ambient host-wide DBus access.
- First-phase **session filesystem access** uses the real host/project filesystem with session cwd, environment, and capability grants. A virtual filesystem remains a possible later layer, not a current requirement.
- The **session environment** is mutable power-user state. Programs inherit it at launch; session-aware tools may observe or change it, while already-running external processes may not automatically reflect every change.
- The **power-user default** is no confirmation nagging for explicit session actions. Prefer clear status, logs, history, undo/recovery where practical, and opt-in guardrails.
- Agents are treated like other same-session apps for trust and capabilities, but their requested and performed actions should be recorded in a session-local **agent action log**.
- A **presentation surface** presents one **session** at a time, and may show any number of that session's **panes**.
- Pane visibility, placement, dragging, animation, and other presentation behavior belong to the **presentation surface**.
- A **presentation surface** may save layout or visibility preferences into the **workspace**, but those saved preferences are not pane state.
- Pane producers may send **presentation requests**. The **presentation surface** may honor, adapt, defer, or reject them according to its own constraints.
- A **presentation window** may be destroyed while one or more **managed apps** continue running.
- **Detach** is the default close behavior when there is session state worth preserving.
- A **hosted Wayland app** is a **managed app** connected to Manifold's nested Wayland compositor.
- A **pane producer** may create, fill, update, or remove one or more **panes**. **Pane clients**, **managed apps**, and **WASM pane apps** are different producer shapes.
- A **WASM pane app** can create any number of **panes** without being one pane itself.
- An external native program can create **panes** by creating Wayland windows that Manifold maps into the workspace.
- **Local presentation indirection** uses DMA-BUF-compatible surface sharing where available, with copied-image correctness fallback. Remote transports preserve per-window streams and client-side composition.
- The **WASM app host** should talk to Manifold through workspace, scene, event, storage, mailbox, swapchain, and capability APIs rather than Smithay, WGPU objects, or renderer internals.
- A **plugin** may contain multiple **plugin actors**, actor-scoped **plugin workers**, and optional **sync policy lanes**.
- A **plugin actor** is the public address-space and mailbox boundary. It is dormant between events; once invoked its dispatcher must not suspend or block, and instead submits work and handles **completion** messages.
- A **plugin worker** shares its actor's memory and capabilities, may use blocking-looking WASI, has no public mailbox, and is cancelled with its actor.
- Worker coordination is guest-owned shared memory plus atomics. Host **notifications** and **pollables** provide wakeup and worker `select`; the actor receives events rather than waiting on worker locks.
- Use another **plugin actor** for independent isolation, addressing, capabilities, or lifecycle. Use a **plugin worker** for cheap shared-state concurrency.
- **Mailbox** delivery is the normal path for durable plugin work; **swapchains** are the latest-state path for sync policy decisions.
- **Sync policy lanes** are only for immediate host pre-commit decisions and must not block on actor queues or mutate host state directly.
- The presentation client owns final local shortcut routing, accepted viewport layout, native chrome, focus, hit testing, and WGPU rendering. The backend owns session and managed-window lifecycle and validates lease-authorized Wayland configuration and input.

## Flagged Ambiguities

- "hosted app" is too narrow. Use **managed app** for the product concept, and **hosted Wayland app** for the nested Wayland implementation.
- "managed app" should not be used for WASM-hosted UI. Use **WASM pane app** for sandboxed UI modules whose presentation state may live in the client and whose coordinated state may live in a session actor.
- "pane" should not imply the state owner. A pane is the presentation unit; the state may live in a plugin, WASM module, external process, session model, browser runtime, or other producer behind indirection.
- "plugin" should not imply one actor, one pane, one worker, or direct compositor authority. A plugin package can expose multiple actors, each with private workers, plus sync policy lanes, pane producers, and UI views behind host-owned boundaries.
- "worker" should not imply a second actor or an independently addressable service. Workers are private shared-memory execution contexts inside one actor. Cooperative workers do not promise multicore parallelism.
- "layout" should not imply session-owned pane state. Layout, visibility, dragging, and animation are presentation-surface concerns unless explicitly saved as workspace preferences. Plugins may propose layout through sync policy lanes, but the host owns the accepted layout.
- "presentation request" should not imply authority. Producers can ask for presentation behavior, but the presentation surface decides what is possible and appropriate.
- "session trust boundary" is not a claim of internet-safe multi-user security. It is the first-phase local trust model for isolating sessions from each other.
- "DBus proxy" should mean session-scoped mediation. Avoid implying unmanaged access to the host user's global DBus session.
- "session filesystem access" should not imply a virtual filesystem exists today. Use real filesystem access first; revisit VFS if the benefits become concrete.
- "session environment" should not be treated as immutable configuration. It is mutable session state, with the usual power-user consequences.
- "power-user default" should not mean silent failure. Avoid nagging prompts by default, but keep actions observable and failures explainable.
- "agent" should not imply a separate trust boundary. Treat agents like apps inside the session, with extra action logging for traceability.
- "agent action log" should not imply cross-session visibility by default. Workspace-level indexing may exist for user inspection, but the log belongs to the session.
- "client" previously meant a Wayland client, a detachable display process, and sometimes a presentation window. Use **Wayland client**, **presentation client**, **presentation endpoint**, or **presentation window** explicitly in durable prose.
- "frontend" remains a legacy/control-command spelling in the MVP tools. In docs, use **presentation endpoint** for the component and **presentation window** for the desktop window.
- "server" and "daemon" are implementation-adjacent. Use **session process** for Manifold's authoritative state owner unless discussing protocol direction or process names.
- "shared implementation" is premature. Use **shared behavior spec** when we mean common Manifold semantics across backend work, and reserve shared code for a later decision.
- "workspace" can be ambiguous between the product umbrella and the concrete durable runtime object. Prefer **workspace** for the product-level environment and **session** for the implementation-level durable unit unless the document is explicitly talking about both layers at once.
