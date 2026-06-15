# Clients

Native presentation endpoints will live here.

The first separate endpoint is expected to be a local Linux/WSL-oriented compositor process. It should open a window, forward input and metrics to the session process, import surfaces, cache scene data, and present frames at refresh rate.

The presentation endpoint should not own authoritative editor, session, app, or pane state. It owns presentation behavior such as layout, visibility, animation, dragging, and hit testing, and may save preferences back to the workspace.
