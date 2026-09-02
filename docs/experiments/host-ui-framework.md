---
status: experiment
type: design-experiment
authority: design
last-reviewed: 2026-09-02
---

# Host UI Framework Experiment

A future experiment should test a presentation client which owns a proper UI
framework, potentially GPUI, and accepts revisioned plugin descriptions of
host-managed UI. The experiment is intentionally not the current plugin UI
target and does not block [[../targets/systems/plugin-pure-renderer]].

The experiment should measure what is gained from host ownership of component
layout, input, accessibility, animation, text, and lifecycle, and what visual
or behavioral flexibility is lost. It must also expose the real protocol cost:
component identity, tree patching, event routing, framework-version evolution,
custom drawing escape hatches, recovery, and reconciliation with actor state.

GPUI is an implementation candidate, not a plugin ABI. An experiment may use
it inside the host, but should not assume that GPUI Rust types, object lifetime,
or event-loop ownership can cross the Component Model boundary. The earlier
conflict with an in-process Smithay-managed presentation window should be
retested against the current external presentation-client architecture rather
than treated as permanently settled.

A useful spike should implement the same nontrivial pane in both the pure
renderer and host-framework models, then compare resize fidelity, input and
accessibility behavior, plugin code size, protocol traffic, recovery, host API
surface, and the effort required for one design outside the framework's normal
component vocabulary.
