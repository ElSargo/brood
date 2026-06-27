# Apps

Repo-local apps that run inside Manifold's nested compositor.

- `manifold-session-switcher.py`: GTK/libadwaita session switcher. It registers over Manifold DBus as presentation chrome, requests left-sidebar placement, lists sessions, creates sessions, and activates sessions.

Built-in WASM apps will live here later. They should use host capabilities and emit scene patches rather than drawing pixels directly.
