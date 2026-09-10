# Complaint capture implementation

## 2026-09-10

Implemented the presentation-owned complaint bundle and the first interactive
capture path:

- `complaints.capture` is a catalogue action with a remappable global shortcut;
- invocation latches backend, session, focus, visible panes, layout revision,
  dimensions, scale and presented frame sequence before accepting text;
- the in-client editor retains its draft on Escape, resumes the same capture,
  accepts Backspace, and durably saves on Enter;
- bundles use stable directories, atomic file replacement and directory fsync;
- note, manifest and bounded presentation diagnostics are ordinary files;
- omitted, unavailable and partial evidence are explicit in the manifest;
- repeated saves reuse the complaint identity and do not duplicate the bundle.

The current renderer does not expose its final surface texture for readback, so
the screenshot collector records `unavailable` instead of misrepresenting a
daemon automation screenshot as the composed presentation. A proper
presentation-owned intermediate composition target/readback remains required.

Save-and-investigate currently records the investigation as unavailable after
the durable save. The requested agent introspection portion is intentionally
deferred. Future work must connect the saved bundle to the existing
session-owned agent lifecycle, add later observations with timestamps, and
preserve report delivery across reconnects without duplicating investigations.
