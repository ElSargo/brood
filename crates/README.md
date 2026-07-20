# Crates

Shared Rust crates live here. `manifold-cli` owns command-line types, DBus client access, server discovery, and the initial presentation handshake. `manifold-ipc` owns the fixed-layout same-device shared-memory lanes and their latency benchmark. `manifold-dmabuf` owns the narrow Linux DMA-BUF-to-WGPU import boundary and its real GBM/Vulkan proof. `manifold-layout` owns compositor-independent viewport layout validation, projection, hit testing, and native fallback behavior.

Additional crate areas:

- session process
- protocol types
- presentation endpoint
- renderer
- pane client SDK
- WASM app host
- built-in app host APIs

No crate layout is final yet.
