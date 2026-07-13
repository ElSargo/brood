# Geometry Layout Policy Plugin

This core-WASM policy-provider plugin is the observable geometry conversion
proof. Its `workspace` key provider consumes a real key event and sends a typed
`SetResolvedLayout` host message that places pane 1 on the left and pane 2 on
the right. The host validates that normalized layout before converting it to
signed logical pixels for rendering and hit-testing.

Run `tools/test-geometry-conversions-nested.sh` to build and load the plugin,
map a real Weston terminal and Weston eventdemo, inject the activation key, and
then send pointer motion, click, wheel-v120, and fractional continuous scroll
through the accepted plugin-owned rectangles. The proof bundle includes the
composed screenshot, state manifest, compositor metrics, and eventdemo's local
input log.
