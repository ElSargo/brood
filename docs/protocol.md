# Protocol Notes

The Manifold protocol connects the local session process, pane clients, and WASM app hosts. A separate presentation endpoint protocol is deferred until Manifold needs a split presenter process.

This document records the intended shape only. Encoding, transport, schemas, and versioning remain undecided.

## Streams

- control stream: session, pane, focus, command, lifecycle, and capability messages
- input stream: keyboard, pointer, touch, IME, clipboard, and window metrics
- scene stream: semantic scene commits and invalidations
- surface stream: handles and metadata for shared GPU or shared-memory surfaces
- app stream: capability-scoped calls between WASM apps and host APIs

## Scene Commits

Scene commits should describe retained or patchable primitives:

- text runs
- rectangles, borders, clips, layers, and transforms
- virtual lists and trees
- input controls
- markdown blocks
- image references
- surface slots
- animation intents

The presentation endpoint may cache and animate scene state without asking the session process for every frame.

## Surface Commits

Surface commits describe external content produced by pane clients:

- browser frames
- terminal surfaces
- Wayland client surfaces
- future media or custom native panes

The preferred local Linux path is GPU sharing through EGL/DMA-BUF-compatible handles. CPU/shared-memory fallback must exist for correctness and unsupported drivers.

## Latency Target

For local sessions, the target is:

- input to visible committed result: p95 at or under 8 ms, p99 at or under one 60 Hz frame
- presentation-endpoint-only animations and compositing: refresh-rate paced

These numbers are targets for future benchmarking, not yet validated facts.
