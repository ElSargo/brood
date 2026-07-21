# Clients

Native presentation endpoints will live here.

`manifold-client` is the first separate endpoint. It opens a WGPU window and owns connection startup: it connects to the session server, starts one when necessary, and completes the presentation protocol handshake. It can also start deliberately without a server, preserving its local renderer and presentation actors while backend inventory, frames, and input forwarding remain unavailable. The target local transport registers atomic per-window surface-tree frames through the descriptor broker, then carries frame sequences and input through bounded shared-memory lanes with eventfd wakeups. The client imports those frames into WGPU, invokes and validates its own WASM flat-layout policy, owns final scene composition and hit testing, and sends only the managed pane's requested logical size back to the session server. Placement never crosses that boundary.

The local development path exports each managed window separately. The server
composes one synchronized Wayland surface tree—including its subsurfaces and
popups—into a reusable DMA-BUF per window, publishes those resources with their
pixel format, source color space, and application identity as one atomic scene
batch, and leaves final placement and color conversion to the WGPU client. The
client imports sRGB-encoded buffers through sRGB texture views, composes in
linear light, and labels its operating-system surface as sRGB so the window
system can apply the current monitor profile. It uses a one-refresh swapchain
latency target and mailbox presentation where supported; copied-image fallback
remains required by the target design.

The presentation endpoint should not own authoritative editor, session, app, or pane state. It owns presentation behavior such as layout, visibility, animation, dragging, and hit testing, and may save preferences back to the workspace.

The client maps local plugin panes and received managed windows together with
the selected flat layout. One client-owned focus target drives input: keys are
first offered to the client-local synchronous key-policy provider with an
immutable snapshot of that combined pane inventory. Consumed focus and
fullscreen actions stay local, plugin messages remain local, and application or
session commands use the asynchronous backend control worker. Only keys the
policy forwards reach the focused plugin or managed window.
Pointer routing uses the same client-owned layout: plugin events stay in the
local actor runtime, while hosted-window records carry the selected pane ID and
pane-local coordinates. The server resolves only that pane's Wayland surface
tree and never repeats hit testing against a server-side placement.

The client also owns a `manifold-wasm` presentation runtime and its Tokio
executor. Presentation-scoped Component and Preview 1 actors can be loaded with
explicit instance identities at client startup. They share one client-owned
filesystem watcher service, run independently of the session daemon, are
periodically observed from the Winit loop, and are joined before the client
executor is dropped. Package placement metadata, local input/layout event
routing and policy lanes share this ownership boundary. Additional
cross-runtime capability routing remains a later layer.

Component presentation actors can submit bounded canvas packets for their pane.
The client retains the latest accepted packet, lowers it to a CPU canvas, uploads
the result into its existing WGPU composition pass, and routes pane-local resize,
focus, pointer, scroll, key, and text events back to the actor. Native text
layout is a separate client host function; packets only place immutable layout
handles. The executable workload is in `examples/wasm-plugin-graphics`, and
`ClientArgs` in `crates/manifold-cli/src/cli.rs` remains the source of truth for
the client invocation.

`F6` remains a temporary recovery binding that toggles focus between a local
plugin pane and the first backend-managed window. User-facing workspace
bindings come from the selected client-local key-policy provider.
