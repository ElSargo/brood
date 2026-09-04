---
status: friction
type: design-friction
authority: observation
last-reviewed: 2026-09-02
---

# Live resize element stability

The resize target requires borders and pane content to move together, but it
does not state whether fixed-size content inside a live-resized pane may be
temporarily scaled while native content catches up. The current native mode
projects the last complete pane texture under new divider geometry and later
promotes a coordinated native redraw. That preserves complete coverage and
cross-pane cohort consistency, but fixed UI elements visibly change size and
position between those promotions.

The presentation-rate new-session-button oracle provides direct evidence. In
the retained fast Cua reproduction at `/tmp/brood-cua-element-repro-final`,
13-22 of 42-43 presented samples per run violated a 0.25 px lock tolerance.
Maximum position error was 3.238 px and maximum width error was 2.530 px. The previous
grader still awarded 100/100 because monotonic forward projection is valid
under its pane-level metrics.

The unresolved design question is what the live phase promises. If fixed-size
chrome and text must remain geometrically locked, whole-pane texture projection
cannot be the normal visible fallback. The implementation needs either
presentation-owned stable element layers, source content that is current for
every presented target, or a different decomposition that projects only
content whose scaling is visually acceptable. The acceptable position, size,
and presentation-cadence tolerances should be recorded in the resize target
before the oracle becomes a general acceptance contract.

A correlated timing capture at `/tmp/brood-resize-pipeline-profile` narrows the
tradeoff. Session-switcher guest drawing takes only 0.181-0.231 ms at p50, and
host validation plus accepted-to-ready takes another 0.374-0.413 ms. The
dominant intervals are resize-event-to-actor-dispatch and
graphics-ready-to-composition. This makes host-managed elements a design
option rather than a demonstrated performance necessity. The current ordinary
resize path also permits target geometry to present before the actor token
covering that exact event watermark; optimizing drawing alone cannot turn
that non-atomic relationship into a guarantee.

## Selected Direction After Architecture Review

The current target [[../targets/systems/plugin-pure-renderer]] selects immutable
application publication, host-invoked render/input facets, and exact paired
geometry/content/interaction commits. This addresses the policy question above
without selecting host-owned widgets or whole-pane projection. The original
measurements remain evidence; implementation and responsiveness are unproven.
A 0.25 physical-pixel committed-geometry oracle must be accompanied by explicit
latency and frame-repetition budgets so stale geometry cannot pass indefinitely.
