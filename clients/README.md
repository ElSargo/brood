# Clients

Native presentation endpoints will live here.

`manifold-client` is the first separate endpoint. It opens a WGPU window and owns connection startup: it connects to the session server, starts one when necessary, and completes the presentation protocol handshake. The opt-in `MANIFOLD_DMABUF_HAPPY_PATH=1` development path takes the latest single-plane hosted DMA-BUF over the typed DBus descriptor broker, imports it into Vulkan WGPU, and composites it fullscreen. Shared-memory frame updates, input forwarding, general scene mirroring, and fallback behavior follow.

The presentation endpoint should not own authoritative editor, session, app, or pane state. It owns presentation behavior such as layout, visibility, animation, dragging, and hit testing, and may save preferences back to the workspace.
