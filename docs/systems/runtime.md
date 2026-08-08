---
status: canonical
type: system
authority: product
last-reviewed: 2026-07-24
code-links:
  - crates/broodd/src/lib.rs
  - crates/broodd/src/runtime.rs
  - clients/brood-client/src/main.rs
  - crates/broodd/src/config.rs
  - crates/broodd/src/session_runner.rs
---

# Runtime

Brood has two runtime roles: an authoritative session server and an
independent presentation client. The server is coherent with zero connected
clients and never creates a native or offscreen presentation target.

Exact commands and parser behavior remain defined in the CLI code; this page
describes ownership and startup order.

## Session Server

The session server:

- creates the nested Wayland display and session catalogue;
- starts the control endpoint and publishes its connection environment;
- starts the daemon actor realm independently of the session catalogue;
- starts session init actors and configured durable applications;
- accepts presentation-client handshakes and grants bounded protocol lanes;
- publishes revisioned pane inventory and per-window frame streams;
- applies lease-authorized pane focus, input, and requested content sizes;
- keeps hosted applications alive when every presentation client disconnects.

It does not own a viewport, presentation window, layout tree, chrome, global
hit testing, or presentation-local key policy.

## Presentation Client

The presentation client:

- discovers or starts a session server and completes the presentation handshake;
- creates a generously sized Winit/WGPU viewport suitable for the default
  multi-pane workspace;
- starts the client init actor, presentation plugins, and selected key policy;
- combines local plugin panes with backend-managed pane inventory;
- grants and recursively composes layout leases;
- imports subscribed window frames and renders the accepted layout;
- performs hit testing, local focus, and physical input routing;
- sends only pane-local input and requested content sizes to the server.

A fresh normal launch does not create a surface session as a side effect of
connecting the presentation. The initial catalogue is empty, the session
switcher is visible, and no terminal or other session-scoped pane exists until
the user creates or selects a session. Presentation-global chrome, including
the shortcut status bar, remains available in this empty state.

Closing the client revokes its presentation authority. Starting another client
creates a new presentation and layout-lease generation without restarting the
session server or its applications.

## Session Runners

A session runner prepares a project environment before the server launches
session-owned applications. Runner activation does not absorb the presentation
client into the server process.

## Related Pages

- [[presentation-client]]
- [[layout-actor]]
- [[input-shortcuts]]
- [[configuration]]
- [[testing]]
- [[../workflows/run-brood]]
