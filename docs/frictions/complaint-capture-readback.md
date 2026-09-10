# Complaint capture needs a presentation readback surface

The complaint target requires the exact pre-editor composed viewport, including
client-local plugins and presentation chrome. The native client currently
renders directly into the acquired presentation surface and that surface is not
configured or abstracted as a reusable readback source. Server computer-use
captures exclude client-local composition and therefore cannot satisfy the
contract.

The durable capture implementation reports the screenshot as unavailable until
the renderer composes through an immutable, copyable presentation-owned target
or provides an equivalent bounded readback callback. That callback must latch
the already-presented frame and must not resize, focus, or acquire automation
control.
