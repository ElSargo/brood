# Vision

Manifold is a persistent graphical workspace for software development.

It should combine the durable session model of terminal multiplexers with a native graphical compositor capable of editor text, browser surfaces, terminals, agent workflows, git tools, rich documents, and eventually nested Wayland applications.

## Goals

- Preserve workspace and managed app state across presentation window close/reopen.
- Make local presentation indirection feel native by keeping surfaces GPU-resident where possible and presenting them with minimal added latency.
- Make browser, terminal, agent, git, editor, and app panes behave consistently.
- Render with native smoothness: high-refresh compositing, smooth scrolling, animations, and low input latency.
- Treat browser and Wayland surfaces as first-class composited surfaces, not native child windows.
- Allow built-in and user apps to run outside the core binary, preferably as sandboxed WASM modules using host capabilities.

## Non-Goals For The First Phase

- Do not build a general-purpose desktop window system.
- Do not stream full frames as the primary local protocol or normal local presentation path.
- Do not target remote connections, remote display, networking, auth, or internet-safe multi-user security.
- Do not make Electron or Chromium the root compositor.
- Do not lock the project into CEF, EGL, Vulkan, WGPU, or any specific IPC before targeted spikes.
- Do not require a separate presentation endpoint process in the first local prototype.

## Product Feel

Manifold should feel immediate. Local typing, pane focus, scrolling, and browser interaction should land within a frame whenever possible. The preferred path keeps browser, Wayland, and other producer surfaces in GPU memory through platform-native sharing or presentation mechanisms; CPU upload and copy-heavy paths are correctness fallbacks, not the target feel. Closing the presentation window should detach when there is state worth preserving; reopening should make managed apps feel as though their regular window was still there.
