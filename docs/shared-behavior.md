# Shared Behavior Spec

This document records the Manifold behavior that supported backend work should satisfy, even when compositor, renderer, windowing, or presentation mechanisms differ.

Unless a section explicitly says otherwise, references here to the implementation, backend, or default behavior mean the Linux-native Manifold path.

## Principles

- Manifold preserves workspace meaning above backend mechanics.
- The product-level workspace contains sessions; a presentation surface presents one session at a time.
- Surfaces should remain GPU-resident whenever the platform can support it.
- Presentation should add as little latency as possible between producer commits, input, and visible output.
- Fallbacks are correctness paths, not the architecture Manifold is trying to optimize for.
- Tests should validate user-observable behavior before shared runtime code exists.

## Workspace And Sessions

The workspace is the whole managed Manifold environment at the product level. A session is the concrete durable runtime unit: an isolated collection of related panes, producer state, environment, cwd, agents, programs, and capabilities.

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
- Plain first-phase sessions may use the real host/project filesystem through session cwd, environment, and granted capabilities.
- Isolated sessions should be able to add filesystem overlays, network namespaces, PID/IPC namespaces, and DBus proxies without changing the pane or presentation model.
- Manifold uses power-user defaults: explicit session actions should not trigger confirmation prompts by default.
- Prefer observable effects, logs, history, undo/recovery where practical, and opt-in guardrails over nagging confirmations.
- Agents are same-session apps for trust and capability purposes, but their requested and performed actions should be logged inside the session for traceability.

See [Session Environments](session-environments.md) for the target `enter`, `show`, filesystem overlay, network namespace, and DBus proxy model.

## Entered Session Environment

An entered session is a shell or command environment bound to one Manifold session.

Expected behavior:

- `manifold enter` creates or reuses a session environment when invoked from outside an entered session, sets the session command context, points GUI clients at the session's nested Wayland display, and then starts a shell or requested command.
- The session server records directory-associated sessions and a directory default so repeated `enter` and `show` calls from the same directory reuse the same session by default.
- Bare `enter` should refuse to nest when the shell is already inside a session; `enter --new` should force a new session and print an easy session identifier without moving the directory default unless requested.
- Session-aware commands infer their target from the entered environment unless explicitly overridden.
- `manifold show` fronts, attaches, or switches presentation to the directory-bound or explicitly selected session, or creates an empty session when invoked outside one and no binding exists, without changing the shell environment.
- `manifold hide` resolves the same session target shape as `show` and detaches the presentation surface when one exists, without closing the session.
- Session listing should show the live sessions associated with the current directory by default and all live sessions when requested globally.
- A thin control alias such as `mctl` may exist, but it should use the same command semantics and default target as Manifold control commands.
- Entered shells should make the active DBus control endpoint, session id, session name, Wayland display, and optional DBus/filesystem isolation paths observable for diagnostics.
- Entering a session should not by itself require filesystem, network, or PID isolation; those are profiles layered on top.

## Session Isolation

Session isolation is for reducing accidental interference between local development tasks and parallel agents. It is not a promise of internet-safe multi-user security.

Expected behavior:

- A filesystem-isolated session writes through a session-owned overlay and can report changed files, diffs, promoted paths, discarded paths, and system-path writes separately from repo writes.
- A network-isolated session has its own loopback namespace so multiple sessions can bind the same local ports.
- Containers or tools launched with host networking from inside a network-isolated entered session should share the session network namespace rather than the ambient host namespace.
- A DBus-isolated session uses a session-local bus or filtered proxy; access to host bus names and cross-session bus names requires explicit grants.
- A PID/IPC-isolated session can report and clean up processes that belong to the session without relying on ambient host process discovery.
- Deleting, exporting, or archiving a session should preserve enough metadata to understand its filesystem, process, DBus, and network effects.
- Isolation failures and fallbacks should be visible in status and logs.

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
- Hide is a session-preserving detach operation, not a quit or close operation.
- Repeated attach or detach commands are idempotent.
- The presentation surface decides which panes are visible and how they are arranged, dragged, animated, stacked, or hidden.
- The presentation surface may save its layout or visibility preferences into the workspace.

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

## Session Persistence

The session process owns durable session truth within the wider workspace environment.

Expected behavior:

- Presentation failure should be reported without destroying the session whenever possible.
- Managed apps that keep running while detached should remain visible in workspace status.
- Browser processes tracked by the session should be cleaned up on session quit.
- Future WASM app state should persist according to host-controlled storage and explicit reload policy.

## Performance

Native feel depends on staying close to platform presentation paths.

Expected behavior:

- Prefer GPU-resident surfaces through DMA-BUF or equivalent platform-native handles.
- Avoid full-frame CPU streaming as the primary local presentation path.
- Keep input-to-visible-result and commit-to-visible-result latency within a frame whenever practical.
- For native nested clients, prefer a late-latched ordering where host input is forwarded, the client commits, Manifold composites, and Manifold submits within one output frame when the client can meet the deadline.
- Report fallback use, frame cadence, and latency in diagnostics so performance failures are explainable.

## Nested Development

Manifold should be able to host a development Manifold instance inside a stable Manifold instance. This is both a product stress test and a practical development workflow.

Expected behavior:

- A stable instance can launch a dev instance as a managed app.
- The dev instance can expose its own DBus control endpoint and nested Wayland display.
- The dev instance can launch and interact with its own managed apps while presented inside the stable instance.
- Stable and dev status streams can be sampled together for frame, input, commit, and latency analysis.
- Host-visible captures can include the stable presentation surface with the dev presentation inside it.
- Hot reload failures in the dev instance should not destabilize the stable instance.

See [Nested Development Harness](nested-dev-harness.md) for the initial harness shape.
