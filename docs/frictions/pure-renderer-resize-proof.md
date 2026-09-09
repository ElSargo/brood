# Pure renderer and delayed resize proof

Resolved by explicit user approval on 2026-09-05.

The pure-renderer target explicitly permits rendering the last published view at
new geometry while the actor is busy. The existing resize-proof Slow/Stall
modes instead delay or withhold actor-produced packets for a resize epoch.
Moving that actor state into immutable publications does not preserve the test:
the facet can immediately render the previous identity at the new geometry.
RenderRequest also does not carry the legacy resize epoch.

Consequently, the migrated plugin cannot establish the original delayed cohort
readiness assertion merely by retaining actor timers and gates. Treating its
successful exact-geometry output as proof of delayed readiness would be false.
Returning NoChange across incompatible geometry would violate the contract;
trapping would exercise failure, not a temporarily slow healthy renderer.

The adopted proof splits these concerns. Actor-stall cases prove that rendering
continues from the last immutable publication at exact new geometry. A bounded,
test-only facet completion delay proves that the host waits for both native facet
commits before promoting a terminal resize cohort. The delay is scoped by the
exact `BROOD_TEST_FACET_RENDER_COMPLETION_DELAY=actor-id=milliseconds` fixture
setting and begins after the two startup render candidates. This preserves the
production independence contract while retaining a real delayed-cohort proof.

The resize-proof feature remains opt-in. Legacy Slow/Stall semantics remain the
default; the pure-renderer lane uses the approved split assertions rather than
claiming that actor publication or intent is a native commit.
