# Clients

Native presentation endpoints will live here.

`manifold-client` is the first separate endpoint. It opens a WGPU window and owns connection startup: it connects to the session server, starts one when necessary, and completes the presentation protocol handshake. The target local transport registers atomic per-window surface-tree frames through the descriptor broker, then carries frame sequences and input through bounded shared-memory lanes with eventfd wakeups. The client imports those frames into WGPU, owns final scene composition, and feeds presentation completion plus the real monitor refresh rate back to the session server for Wayland frame pacing.

The local development path exports each managed window separately. The server composes one synchronized Wayland surface tree—including its subsurfaces and popups—into a reusable DMA-BUF per window, publishes those window resources as one atomic scene batch, and leaves final placement to the WGPU client. It uses a one-refresh swapchain latency target and mailbox presentation where supported; copied-image fallback remains required by the target design.

The presentation endpoint should not own authoritative editor, session, app, or pane state. It owns presentation behavior such as layout, visibility, animation, dragging, and hit testing, and may save preferences back to the workspace.
