# Clients

Native presentation endpoints will live here.

`manifold-client` is the first separate endpoint. It opens a WGPU window and owns connection startup: it connects to the session server, starts one when necessary, and completes the presentation protocol handshake. It can also start deliberately without a server, preserving its local renderer and presentation actors while backend inventory, frames, and input forwarding remain unavailable. The target local transport registers atomic per-window surface-tree frames through the descriptor broker, then carries frame sequences and input through bounded shared-memory lanes with eventfd wakeups. The client imports those frames into WGPU, owns final scene composition, and feeds presentation completion plus the real monitor refresh rate back to the session server for Wayland frame pacing.

The local development path exports each managed window separately. The server composes one synchronized Wayland surface tree—including its subsurfaces and popups—into a reusable DMA-BUF per window, publishes those window resources with their pixel format and source color space as one atomic scene batch, and leaves final placement and color conversion to the WGPU client. The client imports sRGB-encoded buffers through sRGB texture views, composes in linear light, and labels its operating-system surface as sRGB so the window system can apply the current monitor profile. It uses a one-refresh swapchain latency target and mailbox presentation where supported; copied-image fallback remains required by the target design.

The presentation endpoint should not own authoritative editor, session, app, or pane state. It owns presentation behavior such as layout, visibility, animation, dragging, and hit testing, and may save preferences back to the workspace.

The client maps local plugin panes and received managed windows together with the deterministic native split layout. One client-owned focus target drives input: plugin events stay local when the plugin pane is focused, while managed-window input is forwarded to the backend under the presentation connection.

The client also owns a `manifold-wasm` presentation runtime and its Tokio
executor. Presentation-scoped Component and Preview 1 actors can be loaded with
explicit instance identities at client startup. They share one client-owned
filesystem watcher service, run independently of the session daemon, are
periodically observed from the Winit loop, and are joined before the client
executor is dropped. Package placement metadata, local input/layout event
routing, policy lanes, and cross-runtime messaging remain later layers over
this ownership boundary.

Component presentation actors can submit bounded canvas packets for their pane.
The client retains the latest accepted packet, lowers it to a CPU canvas, uploads
the result into its existing WGPU composition pass, and routes pane-local resize,
focus, pointer, scroll, key, and text events back to the actor. Native text
layout is a separate client host function; packets only place immutable layout
handles. The executable workload is in `examples/wasm-plugin-graphics`, and
`ClientArgs` in `crates/manifold-cli/src/cli.rs` remains the source of truth for
the client invocation.

`F6` temporarily toggles focus between the local plugin pane and the first backend-managed window. This is explicitly an MVP testing binding while presentation shortcut policy is hosted in the client; it is not the target user-facing keybinding design.
