# Clients

Native presentation endpoints will live here.

`manifold-client` is the first separate endpoint. It opens a WGPU window and owns connection startup: it connects to the session server, starts one when necessary, and completes the presentation protocol handshake. Surface transport, scene mirroring, input forwarding, and composition follow in later stages.

The presentation endpoint should not own authoritative editor, session, app, or pane state. It owns presentation behavior such as layout, visibility, animation, dragging, and hit testing, and may save preferences back to the workspace.
