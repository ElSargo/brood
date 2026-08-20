# Session Switcher UI Implementation Notes

Implementations of the [Session Switcher UI Experiment](../session-switcher-ui.md)
should add a short note in this directory before handoff.

Name the note after the SDK or implementation spike being evaluated. Each note
should cover:

- what was implemented and how to run it;
- what real input and rendering behavior was verified;
- what failed or remained incomplete;
- the canvas commands and native text facilities used;
- component, layout, styling, and hit-testing code which remained guest-side;
- places where the SDK was awkward or unexpectedly verbose;
- any missing canvas, text, input, accessibility, or lifecycle facility;
- recommended changes to the boundaries in [Plugin UI](../../targets/systems/plugin-ui.md)
  and [Plugin Graphics](../../targets/systems/plugin-graphics.md).

Be explicit if an implementation appears to require host component knowledge.
The note should first identify the concrete missing primitive rather than
proposing a component ABI.
