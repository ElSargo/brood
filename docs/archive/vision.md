# Vision

Brood is a persistent graphical workspace for software development.

It should combine the durable session model of terminal multiplexers with a native graphical compositor capable of editor text, browser surfaces, terminals, agent workflows, git tools, rich documents, and eventually nested Wayland applications.

Sessions should also become enterable development environments. A developer or agent should be able to enter a Brood session from a shell, launch GUI apps and local services into that session, run session-scoped control commands, and optionally isolate filesystem, network, process, and DBus state so parallel work does not cross-contaminate.

## Goals

- Preserve workspace and managed app state across presentation window close/reopen.
- Make local presentation indirection feel native by keeping surfaces GPU-resident where possible and presenting them with minimal added latency.
- Make browser, terminal, agent, git, editor, and app panes behave consistently.
- Render with native smoothness: high-refresh compositing, smooth scrolling, animations, and low input latency.
- Treat browser and Wayland surfaces as first-class composited surfaces, not native child windows.
- Allow built-in and user apps to run outside the core binary, preferably as sandboxed WASM modules using host capabilities.
- Make `brood enter` create or reuse the default session tied to the current directory when invoked from outside one, and let `brood enter --new` force creation when nested without stealing that default.
- Let `brood show` front or attach the session tied to the current directory, or create an empty one when invoked outside a session and no binding exists.
- Let `brood hide` remove that presentation surface from view without closing the session.
- Make active sessions discoverable from the current directory, with a global session list when needed.
- Support optional session isolation profiles for filesystem overlays, network namespaces, PID/IPC namespaces, and DBus proxies.
- Make parallel agents practical by giving each agent a coherent session capsule with its own panes, ports, DBus scope, filesystem writes, logs, and change export path.

## Non-Goals For The First Phase

- Do not build a general-purpose desktop window system.
- Do not stream full frames as the primary local protocol or normal local presentation path.
- Do not target remote connections, remote display, remote workspace networking, auth, or internet-safe multi-user security.
- Do not make Electron or Chromium the root compositor.
- Do not lock the project into CEF, EGL, Vulkan, WGPU, or any specific IPC before targeted spikes.
- Do not require a separate presentation endpoint process in the first local prototype.
- Do not require full filesystem or namespace virtualization before plain entered sessions work.
- Do not claim hostile-code containment from the first local isolation profiles.

## Product Feel

Brood should feel immediate. Local typing, pane focus, scrolling, and browser interaction should land within a frame whenever possible. The preferred path keeps browser, Wayland, and other producer surfaces in GPU memory through platform-native sharing or presentation mechanisms; CPU upload and copy-heavy paths are correctness fallbacks, not the target feel. Closing the presentation window should detach when there is state worth preserving; reopening should make managed apps feel as though their regular window was still there.

Entering a session should feel like entering a focused development capsule rather than remembering a pile of random socket paths. From inside that capsule, command-line tools, screenshots, input injection, hosted apps, agents, local servers, and `brood ctl` should naturally target the current Brood session unless the user explicitly chooses another one.

See [Session Environments](session-environments.md) for the target command context, isolation, filesystem overlay, network namespace, and DBus proxy model.
