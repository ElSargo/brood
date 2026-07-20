# Clients

Native presentation endpoints will live here.

`manifold-client` is the first separate endpoint. It opens a WGPU window and owns connection startup: it connects to the session server, starts one when necessary, and completes the presentation protocol handshake. The target local transport registers atomic per-window surface-tree frames through the descriptor broker, then carries frame sequences and input through bounded shared-memory lanes with eventfd wakeups. The client imports those frames into WGPU, owns final scene composition, and feeds presentation completion plus the real monitor refresh rate back to the session server for Wayland frame pacing.

The local development path exports each managed window separately. The server composes one synchronized Wayland surface tree—including its subsurfaces and popups—into a reusable DMA-BUF per window, publishes those window resources with their pixel format and source color space as one atomic scene batch, and leaves final placement and color conversion to the WGPU client. The client imports sRGB-encoded buffers through sRGB texture views, composes in linear light, and labels its operating-system surface as sRGB so the window system can apply the current monitor profile. It uses a one-refresh swapchain latency target and mailbox presentation where supported; copied-image fallback remains required by the target design.

The presentation endpoint should not own authoritative editor, session, app, or pane state. It owns presentation behavior such as layout, visibility, animation, dragging, and hit testing, and may save preferences back to the workspace.
