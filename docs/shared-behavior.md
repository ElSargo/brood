# Shared Behavior Spec

This document records the Manifold behavior that backend implementations should satisfy, even when their compositor, renderer, windowing, or presentation mechanisms differ.

The Linux/Rust nested compositor remains the clean reference path for now. The WSLg-native path may satisfy the same behavior through Weston, RDPRAIL, and Windows window integration rather than through Manifold-owned Winit presentation.

## Principles

- Manifold preserves workspace meaning above backend mechanics.
- A workspace contains sessions; a presentation surface presents one session at a time.
- Surfaces should remain GPU-resident whenever the platform can support it.
- Presentation should add as little latency as possible between producer commits, input, and visible output.
- Fallbacks are correctness paths, not the architecture Manifold is trying to optimize for.
- Tests should validate user-observable behavior before shared runtime code exists.

## Workspace And Sessions

The workspace is the whole managed Manifold environment. A session is an isolated collection of related panes, producer state, environment, cwd, agents, programs, and capabilities.

Expected behavior:

- A presentation surface presents one session at a time.
- A session can be configured and managed as a unit.
- Programs and agents inside a session may access the state and capabilities granted inside that session.
- Environment variables, cwd, and similar launch context belong to the session, are mutable, and are inherited by programs launched inside it.
- Session-aware tools may observe and update session environment state; already-running external processes may not automatically reflect changes to inherited process environment.
- Sessions are isolated from each other unless the workspace grants an explicit bridge.
- Session membership is the primary first-phase trust boundary: programs, agents, and WASM apps inside the same session are assumed to trust each other.
- WASM apps inside a session may communicate with each other through session APIs and granted capabilities.
- External apps may use a session-scoped DBus proxy for desktop-style communication inside the session.
- The session DBus proxy should not expose ambient host-wide or cross-session DBus access by default.
- First-phase sessions use the real host/project filesystem through session cwd, environment, and granted capabilities.
- A virtual filesystem may become useful later, but it is not required for the current workspace/session model.
- Manifold uses power-user defaults: explicit session actions should not trigger confirmation prompts by default.
- Prefer observable effects, logs, history, undo/recovery where practical, and opt-in guardrails over nagging confirmations.
- Agents are same-session apps for trust and capability purposes, but their requested and performed actions should be logged inside the session for traceability.

## Pane Identity

A pane is the unit of presentation in a Manifold workspace. Its state is not owned by the presentation surface itself; the presentation surface is an indirect view of state owned by a producer, the session process, or a host-managed model behind it.

Expected behavior:

- A pane can keep its presentation identity across presentation close/reopen when the underlying state still exists.
- A pane can temporarily have no visible presentation surface while its producer keeps useful state alive.
- Replacing the rendering path or backend should not move pane state into the presentation surface.
- Crashed or exited producers should be represented as lifecycle events on their panes instead of silently resurrecting stale surfaces.
- Producer exit should close open panes by default unless an explicit preservation, reload, or recovery policy applies.

## Pane Producers

Pane producers include supervised pane clients, managed app surfaces, and WASM pane apps. A producer is not the same thing as a pane.

Expected behavior:

- Managed native apps are external processes whose surfaces are tracked and placed by Manifold.
- Hosted Wayland apps are managed apps connected to Manifold's nested Wayland compositor; their Wayland windows can create panes.
- WASM pane apps own durable app state in the session process and can create any number of panes by emitting semantic scene patches.
- Browser panes have a current behavior/testing path: real Chromium launched as a hosted Wayland managed app.
- Browser panes also have a roadmap path: a minimal first-party browser pane client designed to show pages with less overhead.

For workspace behavior tests, prefer real Chrome/Chromium because it exercises realistic multi-surface, input, lifecycle, and rendering behavior.

## Focus And Input

Focus belongs to the workspace pane model, even when a backend uses platform-specific surfaces or host windows.

Expected behavior:

- Pointer, keyboard, wheel, and future IME/clipboard events route to the focused or hit-tested pane according to Manifold policy.
- Focus changes are observable through status, events, or testable workspace state.
- Detach and reattach should not lose the intended focused pane when that pane still exists.
- If the focused producer exits, focus should clear or move according to an explicit rule rather than pointing at stale state.

## Attach, Detach, Show, And Hide

Detach removes or hides presentation without treating the session as quit. Pane visibility is detached from pane state.

Expected behavior:

- Closing a presentation window detaches when there is session or managed-app state worth preserving.
- Detach keeps managed apps and pane producers alive when their own lifecycle allows it.
- Reattach presents the latest pane state without requiring managed apps to relaunch.
- Repeated attach or detach commands are idempotent.
- The presentation surface decides which panes are visible and how they are arranged, dragged, animated, stacked, or hidden.
- The presentation surface may save its layout or visibility preferences into the workspace.
- Hide/show semantics for WSLg-native host windows should preserve the same product meaning even if implemented through RDPRAIL or Weston-specific mechanisms.

## Presentation Requests

Pane producers and apps may communicate with the presentation surface using an actor-like request model. A request describes what the producer wants; it is not a command that the presentation surface must obey.

Expected behavior:

- Producers may request presentation behavior such as preferred size, minimum useful size, grouping, transient parentage, modality, attention, visibility, placement, or restoration of previous presentation preferences.
- The presentation surface may honor, adapt, defer, or reject a request according to its own constraints, user preferences, available space, backend limits, and session policy.
- Rejected or adapted requests should be observable enough for the producer, tests, or diagnostics to understand what happened.
- Producers should be prepared for the presentation surface to make a different choice.
- The presentation surface should preserve normal application expectations where practical without giving producers authority over workspace presentation.

## Window Grouping

Manifold may present multiple producer surfaces as one workspace group.

Expected behavior:

- A multi-window app can create multiple panes or be associated with one workspace group when that best matches user intent.
- Popups, auxiliary windows, and browser-created windows should not escape Manifold placement policy by default.
- Backend-specific host windows may differ, but workspace commands should see a coherent pane or group identity.
- The WSLg two-client composite PoC is treated as an implementation experiment toward this behavior, not the final grouping model.

## Session Persistence

The session process owns durable workspace truth.

Expected behavior:

- Presentation failure should be reported without destroying the session whenever possible.
- Managed apps that keep running while detached should remain visible in workspace status.
- Browser processes tracked by the session should be cleaned up on session quit.
- Future WASM app state should persist according to host-controlled storage and explicit reload policy.

## Performance

Native feel depends on staying close to platform presentation paths.

Expected behavior:

- Prefer GPU-resident surfaces through DMA-BUF, WSLg graphics redirection, or equivalent platform-native handles.
- Avoid full-frame CPU streaming as the primary local presentation path.
- Keep input-to-visible-result and commit-to-visible-result latency within a frame whenever practical.
- For native nested clients, prefer a late-latched ordering where host input is forwarded, the client commits, Manifold composites, and Manifold submits within one output frame when the client can meet the deadline.
- Report fallback use, frame cadence, and latency in diagnostics so performance failures are explainable.
