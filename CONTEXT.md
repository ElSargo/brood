# Manifold Context

Manifold is a local persistent graphical workspace that explores whether an extra compositor/session layer can feel native.

## Language

**Workspace**:
The whole managed Manifold environment that contains sessions, saved presentation preferences, configuration, and shared project context.
_Avoid_: single session, presentation window

**Session**:
An isolated collection of related panes, producer state, environment, cwd, agents, programs, and capabilities that can be presented, configured, and managed together.
_Avoid_: presentation surface, layout

**Session trust boundary**:
The assumption that programs, agents, and WASM apps inside the same session may discover and communicate with each other according to session capabilities.
_Avoid_: cross-session ambient trust, internet-safe sandbox boundary

**Session process**:
The authoritative local process that owns durable workspace/session state.
_Avoid_: server, daemon, display server when not discussing a protocol role

**Pane**:
A unit of presentation in a Manifold workspace whose state lives behind a layer of indirection rather than in the presentation surface itself.
_Avoid_: app, process, native child window, tab

**Pane producer**:
Anything that can create or supply content for one or more panes, including a supervised pane client process, a managed app surface, or a WASM program.
_Avoid_: plugin, renderer

**Pane client**:
A supervised process that produces scene data or surfaces for one or more panes.
_Avoid_: native child window, OS window

**WASM app host**:
The session-process component that loads sandboxed WASM modules, grants capabilities, calls module lifecycle functions, and applies their requested workspace or scene changes.
_Avoid_: plugin runner, renderer

**WASM controller**:
A WASM module that changes workspace behavior through host APIs, such as focus rules, presentation requests, commands, or automation.
_Avoid_: compositor plugin

**WASM pane app**:
A WASM module that owns durable app state in the session process and may create any number of panes by producing scene patches.
_Avoid_: managed app

**Managed app**:
An external native application whose windows or surfaces Manifold tracks, places, and preserves as part of a session.
_Avoid_: captured window, child window, remote app

**Hosted Wayland app**:
A managed app launched under Manifold's nested Wayland compositor.
_Avoid_: generic hosted app when discussing WSLg-native behavior

**Presentation window**:
The normal desktop window opened by a presentation endpoint to show a session on the user's current compositor.
_Avoid_: remote client, framebuffer viewer

**Presentation surface**:
The view that presents one session at a time, choosing which of that session's panes are visible and how they are arranged, animated, and interacted with.
_Avoid_: session, pane owner

**Presentation request**:
A message from a pane producer to a presentation surface describing desired presentation behavior, such as size, grouping, modality, attention, or placement hints.
_Avoid_: layout command, guaranteed placement

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
The component that owns final presentation execution for a session on a backend.
_Avoid_: frontend, display client when the process boundary is not the point

**Detach**:
Closing or removing a presentation window while preserving the session and any managed apps that still have live state.
_Avoid_: quit, close app

**Local presentation indirection**:
The extra local layer between managed app content and the user's desktop compositor, intended to preserve app/session state while still feeling like native windows.
_Avoid_: remote desktop, streaming

**Backend implementation**:
A platform-specific way to host surfaces, route input, pace frames, and present Manifold sessions while preserving the same session semantics.
_Avoid_: fallback path, workaround

**WSLg-native implementation**:
A Windows Subsystem for Linux implementation that integrates Manifold's pane/session behavior at the WSLg compositor layer so managed apps can keep WSLg's accelerated presentation path.
_Avoid_: WSL fallback, SHM-only WSL path

**Shared behavior spec**:
The product-level Manifold semantics that both implementation experiments should try to satisfy, such as pane identity, focus rules, attach/detach behavior, show/hide, session persistence, and window grouping.
_Avoid_: shared runtime, common compositor abstraction

**Capability grant**:
An explicit permission that lets a WASM module use a host API for a bounded purpose.
_Avoid_: ambient permission, unrestricted access

## Relationships

- The current `manifoldd` binary is the MVP **session process** and also owns the MVP **presentation endpoint**.
- A future `manifold-client` may become a separate **presentation endpoint** without owning authoritative session state.
- A **workspace** contains one or more **sessions**.
- A **session** contains related panes, producer state, environment, cwd, agents, programs, and capabilities isolated from other sessions.
- The **session trust boundary** is the primary first-phase trust model: things in the same session are assumed to trust each other, while cross-session access requires explicit workspace-level bridging or grants.
- **WASM pane apps** in the same **session** may communicate with each other through session APIs and capabilities.
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
- **Local presentation indirection** uses DMA-BUF-compatible surface sharing where available, with correctness fallbacks.
- A **backend implementation** may be a nested compositor on normal Linux or a WSLg-integrated compositor/shell on WSL.
- The **WSLg-native implementation** should avoid forcing GPU-capable managed apps through Manifold's SHM-only nested compositor path.
- The Linux/Rust nested compositor and the WSLg/C Weston implementation are separate experiments for now. Share behavior notes and test intent first; defer shared runtime code until the WSLg insertion point and control surface are clearer.
- A **shared behavior spec** is the near-term unification target. The Linux implementation can remain the clean Rust reference while WSLg proves the Windows/RAIL compositor path in C.
- WSLg transport internals do not make Manifold a remote-display product; the goal is GPU-resident local presentation with Manifold session behavior and minimal added latency.
- The **WASM app host** should talk to Manifold through workspace, scene, event, storage, and capability APIs rather than Smithay, Weston, or renderer internals.

## Flagged Ambiguities

- "hosted app" was too narrow for WSLg-native work. Use **managed app** for the cross-backend product concept, and **hosted Wayland app** for the nested Wayland implementation.
- "managed app" should not be used for WASM-hosted UI. Use **WASM pane app** for sandboxed UI modules whose state lives in the session process.
- "pane" should not imply the state owner. A pane is the presentation unit; the state may live in a WASM module, external process, session model, browser runtime, or other producer behind indirection.
- "layout" should not imply session-owned pane state. Layout, visibility, dragging, and animation are presentation-surface concerns unless explicitly saved as workspace preferences.
- "presentation request" should not imply authority. Producers can ask for presentation behavior, but the presentation surface decides what is possible and appropriate.
- "session trust boundary" is not a claim of internet-safe multi-user security. It is the first-phase local trust model for isolating sessions from each other.
- "DBus proxy" should mean session-scoped mediation. Avoid implying unmanaged access to the host user's global DBus session.
- "session filesystem access" should not imply a virtual filesystem exists today. Use real filesystem access first; revisit VFS if the benefits become concrete.
- "session environment" should not be treated as immutable configuration. It is mutable session state, with the usual power-user consequences.
- "power-user default" should not mean silent failure. Avoid nagging prompts by default, but keep actions observable and failures explainable.
- "agent" should not imply a separate trust boundary. Treat agents like apps inside the session, with extra action logging for traceability.
- "agent action log" should not imply cross-session visibility by default. Workspace-level indexing may exist for user inspection, but the log belongs to the session.
- "client" previously meant a Wayland client, a future detachable display process, and sometimes the current presentation window. Use **Wayland client**, **presentation endpoint**, or **presentation window** instead of bare "client" in durable prose.
- "frontend" remains a legacy/control-command spelling in the MVP tools. In docs, use **presentation endpoint** for the component and **presentation window** for the desktop window.
- "server" and "daemon" are implementation-adjacent. Use **session process** for Manifold's authoritative state owner unless discussing protocol direction or process names.
- "WSL path" is ambiguous. Use **WSLg-native implementation** when discussing a compositor-layer integration with WSLg, and use "WSL SHM fallback" only for the current nested `manifoldd` behavior on WSLg.
- "shared implementation" is premature. Use **shared behavior spec** when we mean common Manifold semantics across Linux and WSLg, and reserve shared code for a later decision.
