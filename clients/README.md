# Clients

Native presentation endpoints will live here.

`manifold-client` is the first separate endpoint. It opens a WGPU window and owns connection startup: it connects to the session server, starts one when necessary, and completes the presentation protocol handshake. It can also start deliberately without a server, preserving its local renderer and presentation actors while backend inventory, frames, and input forwarding remain unavailable. The target local transport registers atomic per-window surface-tree frames through the descriptor broker, then carries frame sequences and input through bounded shared-memory lanes with eventfd wakeups. The client imports those frames into WGPU, owns final scene composition, and feeds presentation completion plus the real monitor refresh rate back to the session server for Wayland frame pacing.

The local development path exports each managed window separately. The server composes one synchronized Wayland surface tree—including its subsurfaces and popups—into a reusable DMA-BUF per window, publishes those window resources with their pixel format and source color space as one atomic scene batch, and leaves final placement and color conversion to the WGPU client. The client imports sRGB-encoded buffers through sRGB texture views, composes in linear light, and labels its operating-system surface as sRGB so the window system can apply the current monitor profile. It uses a one-refresh swapchain latency target and mailbox presentation where supported; copied-image fallback remains required by the target design.

The presentation endpoint should not own authoritative editor, session, app, or pane state. It owns presentation behavior such as layout, visibility, animation, dragging, and hit testing, and may save preferences back to the workspace.

The current client maps all received windows with the deterministic native split layout and keeps focus locally. The focused window drives hosted-application keyboard input, and pointer coordinates are projected from the client-owned pane bounds back into the matching backend surface.

The client also owns a `manifold-wasm` presentation runtime and its Tokio
executor. Presentation-scoped Component and Preview 1 actors can be loaded with
explicit instance identities at client startup. They share one client-owned
filesystem watcher service, run independently of the session daemon, are
periodically observed from the Winit loop, and are joined before the client
executor is dropped. Package placement metadata, local input/layout event
routing, policy lanes, and cross-runtime messaging remain later layers over
this ownership boundary.

`Ctrl-J` and `Ctrl-K` temporarily cycle the focused window in the client. This is explicitly a testing hack while the presentation shortcut-policy path is moved into the client; it is not the target user-facing keybinding design.
