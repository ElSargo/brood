---
status: friction
type: design-friction
authority: observation
last-reviewed: 2026-08-29
---

# System profiler coverage gaps

The profiler's native event clock and presentation-actor render correlations
are implemented, but two boundaries cannot yet provide exact attribution.

Managed-window input messages have no causal profiler identity in the product
protocol. Adding one only to diagnostic builds would create a different wire
contract; adding it to the stable message intentionally requires protocol
design. Until then, the client and server spans share a clock but their exact
per-input join relies on workload sequence markers.

WGPU submission completion brackets aggregate queued GPU work. It does not say
which render pass or shader consumed GPU time. Hardware timestamp queries need
device-feature negotiation, bounded readback buffers, delayed asynchronous
mapping, renderer-recovery handling, and a documented fallback for adapters
without timestamp support.
