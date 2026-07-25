# Crates

Shared Rust crates live here. `brood-cli` owns command-line types, DBus client access, server discovery, and the initial presentation handshake. `brood-ipc` owns the fixed-layout same-device shared-memory lanes and their latency benchmark. `brood-dmabuf` owns the narrow Linux DMA-BUF-to-WGPU import boundary. `brood-layout` owns compositor-independent viewport layout validation, projection, hit testing, and native fallback behavior. `brood-graphics` owns the bounded, renderer-independent canvas packet types and codec shared by guests and presentation clients. `brood-wasm` owns Wasmtime configuration, plugin artifact classification, actor and worker execution, runtime I/O, policy providers, and the nested behavior-test harness engine; hosts adapt its typed requests and outcomes without depending on Wasmtime.

Additional crate areas:

- session process
- protocol types
- presentation endpoint
- renderer
- pane client SDK
- built-in app host APIs

No crate layout is final yet.
