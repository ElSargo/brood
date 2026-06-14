# Manifold Context

Manifold is a local persistent graphical workspace that explores whether an extra compositor/session layer can feel native.

## Language

**Session process**:
The authoritative local process that owns durable workspace/session state.
_Avoid_: server, daemon, display server when not discussing a protocol role

**Pane**:
A layout and focus unit in a Manifold workspace.
_Avoid_: native child window, tab

**Pane producer**:
Anything that supplies pane content to Manifold, including a supervised pane client process, a managed app surface, or a WASM app.
_Avoid_: plugin, renderer

**Pane client**:
A supervised process that produces scene data or surfaces for one or more panes.
_Avoid_: native child window, OS window

**Managed app**:
An external native application whose windows or surfaces Manifold tracks, places, and preserves as part of a session.
_Avoid_: captured window, child window, remote app

**Hosted Wayland app**:
A managed app launched under Manifold's nested Wayland compositor.
_Avoid_: generic hosted app when discussing WSLg-native behavior

**Presentation window**:
The normal desktop window opened by a presentation endpoint to show a session on the user's current compositor.
_Avoid_: remote client, framebuffer viewer

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
The product-level Manifold semantics that both implementation experiments should try to satisfy, such as pane identity, focus rules, attach/detach behavior, show/hide, and session persistence.
_Avoid_: shared runtime, common compositor abstraction

## Relationships

- The current `manifoldd` binary is the MVP **session process** and also owns the MVP **presentation endpoint**.
- A future `manifold-client` may become a separate **presentation endpoint** without owning authoritative session state.
- A **presentation window** may be destroyed while one or more **managed apps** continue running.
- **Detach** is the default close behavior when there is session state worth preserving.
- A **hosted Wayland app** is a **managed app** connected to Manifold's nested Wayland compositor.
- A **pane producer** fills one or more **panes**. **Pane clients** and **WASM apps** are different producer shapes.
- **Local presentation indirection** uses DMA-BUF-compatible surface sharing where available, with correctness fallbacks.
- A **backend implementation** may be a nested compositor on normal Linux or a WSLg-integrated compositor/shell on WSL.
- The **WSLg-native implementation** should avoid forcing GPU-capable managed apps through Manifold's SHM-only nested compositor path.
- The Linux/Rust nested compositor and the WSLg/C Weston implementation are separate experiments for now. Share behavior notes and test intent first; defer shared runtime code until the WSLg insertion point and control surface are clearer.
- A **shared behavior spec** is the near-term unification target. The Linux implementation can remain the clean Rust reference while WSLg proves the Windows/RAIL compositor path in C.

## Flagged Ambiguities

- "hosted app" was too narrow for WSLg-native work. Use **managed app** for the cross-backend product concept, and **hosted Wayland app** for the nested Wayland implementation.
- "client" previously meant a Wayland client, a future detachable display process, and sometimes the current presentation window. Use **Wayland client**, **presentation endpoint**, or **presentation window** instead of bare "client" in durable prose.
- "frontend" remains a legacy/control-command spelling in the MVP tools. In docs, use **presentation endpoint** for the component and **presentation window** for the desktop window.
- "server" and "daemon" are implementation-adjacent. Use **session process** for Manifold's authoritative state owner unless discussing protocol direction or process names.
- "WSL path" is ambiguous. Use **WSLg-native implementation** when discussing a compositor-layer integration with WSLg, and use "WSL SHM fallback" only for the current nested `manifoldd` behavior on WSLg.
- "shared implementation" is premature. Use **shared behavior spec** when we mean common Manifold semantics across Linux and WSLg, and reserve shared code for a later decision.
