# Presentation Transport Target Drift

The presentation transport target names protocol version 22 for managed-resize
correlation, while `brood-protocol` currently requires version 24 and the
internal IPC progress record says version 24 includes a later wire correction.
This is documentation drift rather than permission to revise the target during
the networking implementation review.

The target also describes resize state as `LatestState`, but
`InputEvent::WindowResize` is assigned to `Critical` by the semantic protocol.
That may be required because resize epochs and recovery authority are lossless
transitions, while only an uncorrelated viewport update is supersedable. The
target should distinguish those cases, or the lane assignment should be
revisited, in a design-authorized pass.
