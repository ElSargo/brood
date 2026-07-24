# Preview 1 POSIX worker example

This guest is a core `wasm32-wasip1-threads` module used by the Preview 1
compatibility worker runtime. Its primary path is a guest-owned bounded queue
in shared linear memory: workers select over host notification pollables while
the actor continues dispatching, and a release/acquire generation publishes
queue and result state. A separate compatibility case demonstrates that an
ordinary Rust `std::io::Read` can suspend one worker and resume normally.
The workload also gives a worker an explicitly writable preopened directory
and verifies an ordinary `std::fs` open/read/write round-trip through it.

Build it from the repository root:

```sh
nix develop --command cargo build \
  --manifest-path examples/wasm-preview1-worker/Cargo.toml \
  --target wasm32-wasip1-threads \
  --release
```

The result remains a core module at:

```text
examples/wasm-preview1-worker/target/wasm32-wasip1-threads/release/manifold_preview1_worker_example.wasm
```

Do not componentize this artifact. Its imported shared memory, exported worker
entry points, and stack/TLS reservation exports are the compatibility ABI. The
runtime derives each worker's WASI authority from the actor grant while keeping
descriptor tables and worker-local state private to the worker Store.

Load this artifact through the normal plugin runtime when validating its
ordinary WASI behavior. Acceptance should use observable plugin output rather
than an in-process runtime fixture.
