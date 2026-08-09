# Resize proof Component

This deterministic Component is the pane workload for the synchronized-resize
harness. Load the same component directly under one of these presentation actor
instance IDs; the lifecycle instance ID selects its deterministic behavior:

- `resize-proof-fast` submits epoch-tagged content in the resize dispatch.
- `resize-proof-slow` waits 120 ms in an SDK timer worker before submitting.
  This leaves a reliable projected-frame capture window while remaining well
  inside the harness's 250 ms transaction deadline.
- `resize-proof-stall` withholds every tagged epoch until it observes the
  host's untagged degraded-recovery resize, then submits later epochs
  immediately to prove recovery. Superseding motion cannot accidentally bypass
  the timeout fixture.
- `resize-proof-stall-after-first` submits the first tagged epoch immediately,
  then behaves like `resize-proof-stall`. The nested harness uses this mode to
  let its deterministic startup-native transaction complete before forcing the
  later pointer-resize timeout and recovery.

The checked-in build and nested harness produce
`brood_resize_proof_plugin.component.wasm` from the core WASM artifact.

## Pixel decode contract

Every frame is made only from solid rectangles, so its markers are stable
without font or image resources.

- Untagged initial content has background RGBA `(0.04, 0.08, 0.18, 1)`.
- The stall instances delay their first post-timeout untagged recovery by 120 ms.
  That sharp recovery frame has background `(0.08, 0.36, 0.16, 1)` and an
  all-one top barcode, so it cannot be mistaken for initial or tagged content.
- Tagged content for epoch `e` has background channels
  `0.22 + 0.20 * ((e >> shift) & 3)`, with shifts 0, 2, and 4 for red,
  green, and blue.
- The top inner edge contains 64 equal cells encoding the complete epoch,
  least-significant bit at the left. Near-white is one; near-black is zero.
- The bottom inner edge contains 64 cells. Bits 0-31 are `width.to_bits()`;
  bits 32-63 are `height.to_bits()`.
- The right inner edge contains 32 top-to-bottom cells encoding
  `scale.to_bits()`.
- The left rail and centre cross identify the instance: cyan is fast, magenta
  is slow, and amber is stall.
- A near-white outer border names the exact submitted logical extent. Any
  scaling or geometry/content mismatch is therefore visible at every edge.

With `BROOD_TRACE_PLUGIN_INPUT=1`, successful initial, tagged, stalled,
recovery-delayed, and recovery events also appear as `resize-proof-*`
local-message diagnostics addressed back to the originating proof actor. This
also keeps diagnostics safe when the harness starts the actors with `--no-init`.
