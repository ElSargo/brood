# Native Client Plugin

The checked-in `plugins/session-switcher` package implements the session
switcher as a client-realm Component Model plugin. Its blessed init instance
starts one presentation child actor; the child owns the session model, query
and rename editors, selection, scrolling, hit testing, styling, and draw-packet
construction. `manifold-client` supplies only typed session snapshots and
operations, pane input, native text layouts, canvas validation, and final WGPU
composition.

The default config starts this artifact directly. The plugin supports session
filtering, pointer and keyboard activation, creation, rename, close, and
scrolling. The nested proof in
`tools/test-session-switcher-plugin-nested.sh` creates three real sessions,
switches with a real pointer click, creates and renames a session with injected
keyboard input, closes it, and captures the initial and renamed client frames.
It correlates those frames with typed command completions and backend catalogue
snapshots.

The canvas workload uses filled and stroked rounded rectangles, rectangles,
one list clip, and native text layout handles. No component, row, button,
editor, or list identity crosses the guest boundary. The current component
code is deliberately direct: extracting its column, list, row, text-editor,
theme, and resource-cache helpers into a guest SDK is the next useful SDK
slice.

The workload exposed two narrower platform needs:

- session catalogue changes should eventually be signalled by the backend;
  the client currently publishes change-detected snapshots from a bounded
  polling worker;
- accessibility metadata and native text caret/selection queries remain
  necessary before the query editor is complete for accessibility and complex
  text editing.

Neither need requires a host component tree. The canvas, text, input, actor
lifecycle, and typed session-command boundaries were sufficient for the usable
switcher.
