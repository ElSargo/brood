# Crates

Shared Rust crates live here. `manifold-cli` owns command-line types, DBus client access, server discovery, and the initial presentation handshake. `manifold-ipc` owns the fixed-layout same-device shared-memory lanes and their latency benchmark. `manifold-dmabuf` owns the narrow Linux DMA-BUF-to-WGPU import boundary. `manifold-layout` owns compositor-independent viewport layout validation, projection, hit testing, and native fallback behavior. `manifold-graphics` owns the bounded, renderer-independent canvas packet types and codec shared by guests and presentation clients. `manifold-wasm` owns Wasmtime configuration, plugin artifact classification, actor and worker execution, runtime I/O, policy providers, and the nested behavior-test harness engine; hosts adapt its typed requests and outcomes without depending on Wasmtime.

Additional crate areas:

- session process
- protocol types
- presentation endpoint
- renderer
- pane client SDK
- built-in app host APIs

No crate layout is final yet.
