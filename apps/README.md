# Apps

Repo-local apps that integrate with Manifold.

- `manifold-session-switcher.py`: GTK/libadwaita session switcher. In Manifold's presentation realm it registers over DBus as presentation chrome and requests left-sidebar placement. From a normal desktop launcher it stays standalone, lists sessions, creates sessions, and activates sessions without replacing the in-presentation switcher.

Built-in WASM apps will live here later. They should use host capabilities and emit scene patches rather than drawing pixels directly.
