# Network Stack Adversarial Review

## Scope

This review followed the network presentation path through
`brood-transport`, the native client's remote bridge, `brood-remote`, and the
artifact exchange. It compared the implementation with the presentation
transport target and exercised the carrier with invalid identities, oversized
lengths, compression expansion, queue saturation, a stalled frame stream,
untrusted credentials, and the checked-in nested network workload.

## Findings and changes

- Pending scene joins counted compressed bytes. Highly compressible frames
  could therefore expand well beyond the intended 256 MiB client budget during
  decode. The join now accounts for decoded pixel bytes and prunes the oldest
  sequence before decoding when the budget is exceeded. The frame/scene reader
  handoff was also reduced to one envelope because each queued frame may occupy
  the full negotiated 64 MiB; QUIC flow control now absorbs the remaining
  burst instead of a sixteen-frame application queue.
- Repeated pending scene and frame identities silently replaced immutable
  records, and stale scenes could re-enter the join after publication. They are
  now rejected or discarded as appropriate.
- Debug assertions were the only bound on two client-side retained FIFOs.
  Release builds could grow them without limit when a network sender stalled.
  Critical input now reports backpressure at the existing actor-output bound,
  and actor traffic returns a bounded-backlog error.
- Several generation-local zero identities crossed the network validator,
  including handshake generations, backend request IDs, pane/resource IDs,
  frame sequences, and session actor realms. The carrier now rejects them
  before dispatch.
- QUIC TLS had certificate authentication but no application protocol
  identifier. Both endpoints now advertise a protocol-versioned ALPN value.
  Peer-opened streams are limited to the fixed handshake and semantic-lane
  topology; unidirectional streams are disabled. Graceful stream finish and
  peer acknowledgement now have explicit five-second deadlines rather than
  relying on the longer connection idle timeout.
- Generated remote private keys inherited permissive existing file modes. Key
  generation now creates and resets the key to mode `0600` before writing.
- Scene encoding repeatedly acquired the shared frame-resource mutex once per
  window. It now snapshots all resource handles under one acquisition and
  performs pixel access and compression after releasing the registry lock.
- The checked-in network proof's transferred component fixture did not
  acknowledge typed start/stop lifecycle hooks, so its final executability
  check timed out after the network portion had already succeeded. The fixture
  now completes lifecycle hooks, preserving the artifact-load assertion.

## Verification

- `nix develop --command cargo test -p brood-transport`
- `nix develop --command cargo test -p brood-client remote::tests`
- `nix develop --command cargo test -p brood-remote`
- `nix develop --command cargo clippy -p brood-transport -p brood-remote -p brood-client --all-targets -- -D warnings`
- `nix develop --command bash tools/test-network-transport-nested.sh`

The carrier test includes a frame sender blocked by an unread frame stream
while a critical keyboard reset crosses another stream within a two-second
deadline. The client tests also construct five 64 MiB decoded frames with tiny
compressed bodies and verify that the oldest sequence is removed to restore
the 256 MiB join budget.

The nested proof reached `OK remote transport` on all three review runs. Its
first two runs then exposed the stale lifecycle behavior in the transferred
component fixture; after that fixture was corrected, the complete script
passed, including its final `presentation WASM actor check passed` assertion.
