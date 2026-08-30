//! Deadline-aware, actor-scoped render scheduling.
//!
//! This module deliberately uses an integer monotonic timeline. Production
//! code converts `Instant`s at the edge while tests can advance a virtual
//! clock without sleeping.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

fn start_input_profile(actor: &ActorScope, watermark: u64) -> u64 {
    if !brood_profiler::enabled() {
        return 0;
    }
    let profile_id = brood_profiler::next_id();
    brood_profiler::flow(
        brood_profiler::Kind::FlowStart,
        "input",
        "key_received",
        profile_id,
        vec![
            ("actor".into(), actor.name.clone().into()),
            ("generation".into(), actor.generation.into()),
            ("watermark".into(), watermark.into()),
        ],
    );
    profile_id
}

fn step_input_profile(
    kind: brood_profiler::Kind,
    name: &'static str,
    profile_id: u64,
    token: RenderToken,
) {
    if profile_id == 0 {
        return;
    }
    brood_profiler::flow(
        kind,
        "input",
        name,
        profile_id,
        vec![("render_token".into(), token.0.into())],
    );
}

pub const MAX_RENDER_ACTORS: usize = 1_024;
/// A render opportunity is actor-scoped, and the plugin ABI permits one actor
/// frame to update at most sixteen panes atomically.
pub const MAX_RELEVANT_PANES: usize = 16;
pub const DIAGNOSTIC_WARMUP_OPPORTUNITIES: u64 = 60;
pub const MAX_CORRELATION_TRACES: usize = 4_096;
const MAX_RETAINED_FRAME_PANES: usize = 16_384;
const MAX_INTERVAL_HISTORY: usize = 64;
const MAX_DISPATCH_PER_TURN: usize = 64;
const MIN_INTERVAL_NS: u64 = 2_000_000;
const MAX_INTERVAL_NS: u64 = 100_000_000;
const COLD_GUEST_COST_NS: u64 = 2_000_000;
pub const COLD_HOST_COST_NS: u64 = 1_000_000;
const COLD_READINESS_COST_NS: u64 = 500_000;
const MAX_HOST_COST_NS: u64 = 50_000_000;
const ABANDON_BACKOFF_BASE_NS: u64 = 1_000_000;
const ABANDON_BACKOFF_MAX_NS: u64 = 100_000_000;
const MAX_INPUT_CORRELATIONS: usize = 4_096;
const LATE_COMPOSITION_RELEASE_NS: u64 = 1_000_000;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ActorScope {
    pub name: String,
    pub generation: u64,
}

impl ActorScope {
    pub fn new(name: impl Into<String>, generation: u64) -> Self {
        Self {
            name: name.into(),
            generation,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RenderToken(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderHint {
    pub token: RenderToken,
    pub target_presentation_time_ns: u64,
    pub submission_deadline_ns: u64,
    pub relevant_panes: Vec<u64>,
    pub event_watermark: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpportunityPlan {
    pub target_ns: u64,
    pub opportunity_end_ns: u64,
    pub dispatch_ns: u64,
    pub deadline_ns: u64,
}

/// A bounded predictor which adapts to fixed and alternating variable-refresh
/// presentation intervals without allowing one stale observation to survive a
/// surface reset.
#[derive(Clone, Debug)]
#[allow(
    clippy::struct_field_names,
    reason = "the ns suffix documents the integer monotonic timeline at every storage boundary"
)]
pub struct FramePredictor {
    fallback_interval_ns: u64,
    last_presented_ns: Option<u64>,
    intervals_ns: VecDeque<u64>,
    safety_margin_ns: u64,
    reanchor_next_present: bool,
}

impl FramePredictor {
    #[must_use]
    pub fn new(refresh_interval_ns: u64) -> Self {
        Self {
            fallback_interval_ns: refresh_interval_ns.clamp(MIN_INTERVAL_NS, MAX_INTERVAL_NS),
            last_presented_ns: None,
            intervals_ns: VecDeque::with_capacity(MAX_INTERVAL_HISTORY),
            safety_margin_ns: 500_000,
            reanchor_next_present: false,
        }
    }

    pub fn reset(&mut self, refresh_interval_ns: Option<u64>) {
        if let Some(interval) = refresh_interval_ns {
            self.fallback_interval_ns = interval.clamp(MIN_INTERVAL_NS, MAX_INTERVAL_NS);
        }
        self.last_presented_ns = None;
        self.intervals_ns.clear();
        self.safety_margin_ns = 500_000;
        self.reanchor_next_present = false;
    }

    /// Advances the presentation timeline after a same-surface renderer
    /// recovery without treating the recovery pause as a cadence sample.
    pub const fn reanchor_next_presentation(&mut self) {
        self.reanchor_next_present = true;
    }

    pub fn observe_presented(&mut self, presented_ns: u64) {
        if self.reanchor_next_present {
            self.last_presented_ns = Some(presented_ns);
            self.reanchor_next_present = false;
            return;
        }
        if let Some(previous) = self.last_presented_ns {
            let interval = presented_ns.saturating_sub(previous);
            if (MIN_INTERVAL_NS..=MAX_INTERVAL_NS).contains(&interval) {
                if self.intervals_ns.len() == MAX_INTERVAL_HISTORY {
                    self.intervals_ns.pop_front();
                }
                self.intervals_ns.push_back(interval);
            } else {
                self.intervals_ns.clear();
            }
        }
        self.last_presented_ns = Some(presented_ns);
    }

    fn predicted_interval_ns(&self) -> u64 {
        let len = self.intervals_ns.len();
        if len < 3 {
            // A recreated or newly attached surface can emit one long setup
            // interval before steady presentation begins. Do not let one or
            // two observations replace the configured monitor cadence.
            return self.fallback_interval_ns;
        }
        let latest = self
            .intervals_ns
            .get(len - 1)
            .copied()
            .unwrap_or(self.fallback_interval_ns);
        let previous = self.intervals_ns.get(len - 2).copied().unwrap_or(latest);
        let before_previous = self.intervals_ns.get(len - 3).copied().unwrap_or(previous);
        let tolerance = 100_000;
        if latest.abs_diff(before_previous) <= tolerance && latest.abs_diff(previous) > tolerance {
            // A,B,A predicts B. This common VRR pattern is lost by a
            // median-only estimator.
            return previous;
        }
        if len >= 4 {
            let tolerance = self.fallback_interval_ns / 4;
            let skipped_interval = self.fallback_interval_ns.saturating_mul(2);
            let has_fallback = self
                .intervals_ns
                .iter()
                .any(|interval| interval.abs_diff(self.fallback_interval_ns) <= tolerance);
            let has_skip = self
                .intervals_ns
                .iter()
                .any(|interval| interval.abs_diff(skipped_interval) <= tolerance);
            let all_configured_or_skipped = self.intervals_ns.iter().all(|interval| {
                interval.abs_diff(self.fallback_interval_ns) <= tolerance
                    || interval.abs_diff(skipped_interval) <= tolerance
            });
            if has_fallback && has_skip && all_configured_or_skipped {
                // Jittered configured-rate frames mixed with two-cell gaps are
                // missed cadence, not a VRR request to perpetuate 16/32 ms.
                // The exact A,B,A VRR signal above deliberately takes
                // precedence, including 8/16 ms on a 120 Hz fallback.
                return self.fallback_interval_ns;
            }
        }
        let mut sorted = self.intervals_ns.iter().copied().collect::<Vec<_>>();
        sorted.sort_unstable();
        sorted
            .get(sorted.len() / 2)
            .copied()
            .unwrap_or(self.fallback_interval_ns)
    }

    #[must_use]
    pub fn plan(&self, now_ns: u64, guest_cost_ns: u64, host_cost_ns: u64) -> OpportunityPlan {
        self.plan_after(now_ns, guest_cost_ns, host_cost_ns, None)
    }

    /// Plans the earliest feasible opportunity strictly after an actor's
    /// accepted, not-yet-presented target when one exists. Presentation
    /// feedback is asynchronous with respect to event-loop service: without
    /// this actor-local floor, a subscription can re-arm into the same target
    /// cell and supersede work which is already queued for that presentation.
    #[must_use]
    fn plan_after(
        &self,
        now_ns: u64,
        guest_cost_ns: u64,
        host_cost_ns: u64,
        accepted_target_ns: Option<u64>,
    ) -> OpportunityPlan {
        let interval = self.predicted_interval_ns();
        let total_cost = guest_cost_ns
            .saturating_add(host_cost_ns)
            .saturating_add(self.safety_margin_ns);
        let mut target = self.last_presented_ns.map_or_else(
            || now_ns.saturating_add(interval),
            |last| last.saturating_add(interval),
        );
        // Compare on the forward timeline. Subtracting first aliases an
        // infeasible target to zero when `total_cost > target`, which can make
        // a start-of-clock opportunity look dispatchable even though its
        // complete budget does not fit before presentation.
        let earliest_target_ns = accepted_target_ns.map_or_else(
            || now_ns.saturating_add(total_cost),
            |accepted_target_ns| {
                now_ns
                    .saturating_add(total_cost)
                    .max(accepted_target_ns.saturating_add(1))
            },
        );
        if target < earliest_target_ns {
            let shortfall = earliest_target_ns.saturating_sub(target);
            let skipped = shortfall.saturating_add(interval.saturating_sub(1)) / interval;
            target = target.saturating_add(skipped.saturating_mul(interval));
        }
        // The margin is dispatch slack, not host work. Applying it to both
        // dispatch and the submission deadline cancels its effect on the
        // guest budget, so deadline feedback can never make a late actor start
        // earlier relative to its deadline.
        let deadline_ns = target.saturating_sub(host_cost_ns);
        OpportunityPlan {
            target_ns: target,
            // `queue.present` is only an estimate of the OS presentation
            // timestamp. Treat observations in the latter half of the same
            // predicted frame cell as the intended opportunity, while still
            // rejecting a presentation delayed by a complete interval.
            opportunity_end_ns: target.saturating_add(interval / 2),
            dispatch_ns: target.saturating_sub(total_cost),
            deadline_ns,
        }
    }

    pub fn note_deadline_result(&mut self, missed: bool) {
        if missed {
            self.safety_margin_ns = self
                .safety_margin_ns
                .saturating_mul(2)
                .clamp(500_000, 4_000_000);
        } else {
            self.safety_margin_ns = self
                .safety_margin_ns
                .saturating_mul(31)
                .saturating_div(32)
                .max(500_000);
        }
    }
}

impl Default for FramePredictor {
    fn default() -> Self {
        Self::new(16_666_667)
    }
}

#[derive(Clone, Debug)]
enum Phase {
    Idle,
    Armed(ArmedOpportunity),
    Outstanding(OutstandingOpportunity),
}

#[derive(Clone, Debug)]
struct ArmedOpportunity {
    plan: OpportunityPlan,
}

#[derive(Clone, Debug)]
struct OutstandingOpportunity {
    token: RenderToken,
    plan: OpportunityPlan,
    covered_revision: u64,
    event_watermark: u64,
}

#[derive(Clone, Debug)]
struct ActorRenderState {
    subscription: bool,
    requested_revision: u64,
    satisfied_revision: u64,
    admitted_watermark: u64,
    reduced_watermark: u64,
    relevant_panes: BTreeSet<u64>,
    guest_cost_ns: u64,
    host_cost_ns: u64,
    readiness_cost_ns: u64,
    abandonments: u8,
    backoff_until_ns: u64,
    pending_inputs: VecDeque<(u64, u64, u64)>,
    last_dispatch_ns: Option<u64>,
    dispatched_count: u64,
    accepted_count: u64,
    presented_count: u64,
    phase: Phase,
}

impl Default for ActorRenderState {
    fn default() -> Self {
        Self {
            subscription: false,
            requested_revision: 0,
            satisfied_revision: 0,
            admitted_watermark: 0,
            reduced_watermark: 0,
            relevant_panes: BTreeSet::new(),
            guest_cost_ns: COLD_GUEST_COST_NS,
            host_cost_ns: COLD_HOST_COST_NS,
            readiness_cost_ns: COLD_READINESS_COST_NS,
            abandonments: 0,
            backoff_until_ns: 0,
            pending_inputs: VecDeque::with_capacity(MAX_INPUT_CORRELATIONS),
            last_dispatch_ns: None,
            dispatched_count: 0,
            accepted_count: 0,
            presented_count: 0,
            phase: Phase::Idle,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SchedulerMetrics {
    pub dispatched: u64,
    pub accepted: u64,
    pub empty: u64,
    pub rejected: u64,
    pub late: u64,
    pub abandoned: u64,
    pub superseded: u64,
    pub cancelled: u64,
    pub deadline_wakeups: u64,
    pub protocol_errors: u64,
    pub intended_target_misses: u64,
    pub catch_up_bursts: u64,
    pub key_to_present_samples: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ActorSchedulerProgress {
    pub actor: String,
    pub generation: u64,
    pub dispatched: u64,
    pub accepted: u64,
    pub presented: u64,
    pub host_cost_estimate_ns: u64,
    pub readiness_cost_estimate_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Settlement {
    NoChange,
    Presented { estimated: bool },
    Superseded,
    Cancelled,
    Abandoned,
}

#[derive(Clone, Debug)]
pub struct CorrelationTrace {
    pub token: RenderToken,
    pub actor: ActorScope,
    pub target_ns: u64,
    pub opportunity_end_ns: u64,
    pub deadline_ns: u64,
    pub dispatched_ns: u64,
    pub accepted_ns: Option<u64>,
    pub graphics_ready_ns: BTreeMap<u64, u64>,
    pub composition_ns: Option<u64>,
    pub gpu_submission_ns: Option<u64>,
    pub presentation_ns: Option<u64>,
    pub deadline_met: Option<bool>,
    pub target_presented: Option<bool>,
    pub settlement: Option<Settlement>,
    expected_panes: BTreeSet<u64>,
    input_correlations: Vec<(u64, u64, u64)>,
    measured_opportunity: bool,
    readiness_noted: bool,
    composition_released: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcceptedFrame {
    pub token: RenderToken,
    pub actor_frame_revision: u64,
    pub deadline_met: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinishError {
    InvalidToken,
}

#[derive(Debug)]
pub struct RenderScheduler {
    actors: BTreeMap<ActorScope, ActorRenderState>,
    next_token: u64,
    next_actor_frame_revision: u64,
    token_owners: BTreeMap<RenderToken, ActorScope>,
    active_traces: BTreeMap<RenderToken, CorrelationTrace>,
    newest_unpresented: BTreeMap<ActorScope, RenderToken>,
    completed_traces: VecDeque<CorrelationTrace>,
    key_to_present_ns: VecDeque<u64>,
    presentation_intervals_ns: VecDeque<u64>,
    measured_key_to_present_ns: VecDeque<u64>,
    measured_presentation_intervals_ns: VecDeque<u64>,
    last_presentation_ns: Option<u64>,
    current_presentation_measured: bool,
    presented_opportunities: u64,
    measured_accepted_opportunities: u64,
    warmup_late_baseline: Option<u64>,
    measured_target_misses: u64,
    deadline_results: VecDeque<bool>,
    last_dispatched: Option<ActorScope>,
    redraw_requested_through_ns: u64,
    redraw_consumed_through_ns: u64,
    metrics: SchedulerMetrics,
}

impl Default for RenderScheduler {
    fn default() -> Self {
        Self {
            actors: BTreeMap::new(),
            next_token: 1,
            next_actor_frame_revision: 1,
            token_owners: BTreeMap::new(),
            active_traces: BTreeMap::new(),
            newest_unpresented: BTreeMap::new(),
            completed_traces: VecDeque::with_capacity(MAX_CORRELATION_TRACES),
            key_to_present_ns: VecDeque::with_capacity(MAX_INPUT_CORRELATIONS),
            presentation_intervals_ns: VecDeque::with_capacity(MAX_INPUT_CORRELATIONS),
            measured_key_to_present_ns: VecDeque::with_capacity(MAX_INPUT_CORRELATIONS),
            measured_presentation_intervals_ns: VecDeque::with_capacity(MAX_INPUT_CORRELATIONS),
            last_presentation_ns: None,
            current_presentation_measured: false,
            presented_opportunities: 0,
            measured_accepted_opportunities: 0,
            warmup_late_baseline: None,
            measured_target_misses: 0,
            deadline_results: VecDeque::with_capacity(MAX_RENDER_ACTORS),
            last_dispatched: None,
            redraw_requested_through_ns: 0,
            redraw_consumed_through_ns: 0,
            metrics: SchedulerMetrics::default(),
        }
    }
}

impl RenderScheduler {
    pub fn register(&mut self, actor: ActorScope) -> bool {
        if !self.actors.contains_key(&actor) && self.actors.len() >= MAX_RENDER_ACTORS {
            return false;
        }
        self.actors.entry(actor).or_default();
        true
    }

    pub fn set_relevant_panes<I>(&mut self, actor: &ActorScope, panes: I) -> bool
    where
        I: IntoIterator<Item = u64>,
    {
        let panes = panes
            .into_iter()
            .take(MAX_RELEVANT_PANES.saturating_add(1))
            .collect::<BTreeSet<_>>();
        if panes.len() > MAX_RELEVANT_PANES {
            return false;
        }
        let Some(state) = self.actors.get_mut(actor) else {
            return false;
        };
        if state.relevant_panes == panes {
            return true;
        }
        let Some(requested_revision) = state.requested_revision.checked_add(1) else {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return false;
        };
        state.relevant_panes.clone_from(&panes);
        state.requested_revision = requested_revision;

        // Actor atomicity is defined over the currently visible changes.
        // Hidden batch entries remain retained by AtomicActorFrames for later
        // exposure, but must cease to be a composition barrier immediately.
        // Do not add newly visible panes to an in-flight token: the requested
        // revision above obtains one fresh frame for that membership.
        if let Some(token) = self.newest_unpresented.get(actor).copied() {
            let became_invisible = self.active_traces.get_mut(&token).is_some_and(|trace| {
                if trace.composition_ns.is_some() {
                    return false;
                }
                trace.expected_panes.retain(|pane| panes.contains(pane));
                trace.expected_panes.is_empty()
            });
            if became_invisible {
                self.newest_unpresented.remove(actor);
                self.settle_trace(token, Settlement::NoChange);
            }
        }
        true
    }

    pub fn admit_event(&mut self, actor: &ActorScope, requests_render: bool) -> Option<u64> {
        let state = self.actors.get_mut(actor)?;
        let Some(admitted_watermark) = state.admitted_watermark.checked_add(1) else {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return None;
        };
        let requested_revision = if requests_render {
            let Some(requested_revision) = state.requested_revision.checked_add(1) else {
                self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
                return None;
            };
            Some(requested_revision)
        } else {
            None
        };
        state.admitted_watermark = admitted_watermark;
        if let Some(requested_revision) = requested_revision {
            state.requested_revision = requested_revision;
        }
        Some(state.admitted_watermark)
    }

    pub fn request_render(&mut self, actor: &ActorScope) -> bool {
        let Some(state) = self.actors.get_mut(actor) else {
            return false;
        };
        let Some(requested_revision) = state.requested_revision.checked_add(1) else {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return false;
        };
        state.requested_revision = requested_revision;
        true
    }

    pub fn set_subscription(&mut self, actor: &ActorScope, enabled: bool) -> bool {
        let Some(state) = self.actors.get_mut(actor) else {
            return false;
        };
        if enabled && !state.subscription {
            let Some(requested_revision) = state.requested_revision.checked_add(1) else {
                self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
                return false;
            };
            state.requested_revision = requested_revision;
        }
        if !enabled
            && state.subscription
            && matches!(&state.phase, Phase::Armed(_))
            && state.requested_revision <= state.satisfied_revision
        {
            // An armed opportunity is retained after it becomes due so a
            // late event-loop wake can still dispatch it. That retention is
            // only valid while the opportunity still has demand. Retire an
            // opportunity created solely by the subscription at the causal
            // disable transition; otherwise its fixed dispatch time becomes
            // a permanently past Winit deadline while the client is idle.
            // A separately requested revision remains armed and is serviced
            // exactly once.
            state.phase = Phase::Idle;
        }
        state.subscription = enabled;
        true
    }

    pub fn mark_reduced_through(&mut self, actor: &ActorScope, watermark: u64) -> bool {
        let Some(state) = self.actors.get_mut(actor) else {
            return false;
        };
        if watermark > state.admitted_watermark {
            return false;
        }
        state.reduced_watermark = state.reduced_watermark.max(watermark);
        true
    }

    pub fn note_key_input(&mut self, actor: &ActorScope, watermark: u64, at_ns: u64) -> bool {
        let Some(state) = self.actors.get_mut(actor) else {
            return false;
        };
        if state.pending_inputs.len() == MAX_INPUT_CORRELATIONS {
            state.pending_inputs.pop_front();
        }
        let profile_id = start_input_profile(actor, watermark);
        state
            .pending_inputs
            .push_back((watermark, at_ns, profile_id));
        true
    }

    pub fn note_actor_host_cost(&mut self, actor: &ActorScope, measured_ns: u64) -> bool {
        let Some(state) = self.actors.get_mut(actor) else {
            return false;
        };
        let measured_ns = measured_ns.clamp(1, MAX_HOST_COST_NS);
        state.host_cost_ns = if measured_ns > state.host_cost_ns {
            measured_ns
        } else {
            state
                .host_cost_ns
                .saturating_mul(7)
                .saturating_add(measured_ns)
                .saturating_div(8)
                .max(1)
        };
        true
    }

    pub fn arm(
        &mut self,
        actor: &ActorScope,
        now_ns: u64,
        predictor: &FramePredictor,
        host_cost_ns: u64,
    ) -> bool {
        let accepted_target_ns = self.accepted_target_ns(actor);
        if self.actor_has_unreleased_frame(actor) {
            return false;
        }
        let Some(state) = self.actors.get_mut(actor) else {
            return false;
        };
        let has_explicit_demand = state.requested_revision != state.satisfied_revision;
        if !matches!(state.phase, Phase::Idle)
            || (state.relevant_panes.is_empty() && !has_explicit_demand)
            || now_ns < state.backoff_until_ns
            || (!state.subscription && !has_explicit_demand)
        {
            return false;
        }
        state.phase = Phase::Armed(ArmedOpportunity {
            plan: predictor.plan_after(
                now_ns,
                state.guest_cost_ns,
                state
                    .host_cost_ns
                    .saturating_add(host_cost_ns)
                    .saturating_add(state.readiness_cost_ns),
                accepted_target_ns,
            ),
        });
        true
    }

    pub fn retarget_armed(
        &mut self,
        actor: &ActorScope,
        now_ns: u64,
        predictor: &FramePredictor,
        host_cost_ns: u64,
    ) -> bool {
        let accepted_target_ns = self.accepted_target_ns(actor);
        let Some(state) = self.actors.get_mut(actor) else {
            return false;
        };
        let Phase::Armed(armed) = &mut state.phase else {
            return false;
        };
        // Once an opportunity is due, keep its dispatch fixed. Replanning a
        // late wake from `now_ns` would skip the still-usable opportunity and
        // can turn a small event-loop delay into permanent starvation.
        if now_ns >= armed.plan.dispatch_ns {
            return true;
        }
        let replanned = predictor.plan_after(
            now_ns,
            state.guest_cost_ns,
            state
                .host_cost_ns
                .saturating_add(host_cost_ns)
                .saturating_add(state.readiness_cost_ns),
            accepted_target_ns,
        );
        // New causal work may reveal a larger cost and require an earlier
        // start, but merely servicing a continuous event stream must never
        // chase `now` by moving an already armed cutoff later forever.
        if replanned.dispatch_ns < armed.plan.dispatch_ns {
            armed.plan = replanned;
        }
        true
    }

    #[must_use]
    pub fn next_deadline_ns(&self) -> Option<u64> {
        self.actors
            .iter()
            .filter_map(|(actor, state)| match &state.phase {
                Phase::Armed(armed) => Some(
                    armed
                        .plan
                        .dispatch_ns
                        .max(state.backoff_until_ns)
                        .max(self.actor_composition_release_ns(actor).unwrap_or(0)),
                ),
                Phase::Idle
                    if (!state.relevant_panes.is_empty()
                        || state.requested_revision != state.satisfied_revision)
                        && state.backoff_until_ns > 0
                        && (state.subscription
                            || state.requested_revision != state.satisfied_revision) =>
                {
                    Some(state.backoff_until_ns)
                }
                _ => None,
            })
            .min()
    }

    pub fn arm_all(&mut self, now_ns: u64, predictor: &FramePredictor, host_cost_ns: u64) {
        let actors = self.actors.keys().cloned().collect::<Vec<_>>();
        for actor in actors {
            if !self.retarget_armed(&actor, now_ns, predictor, host_cost_ns) {
                self.arm(&actor, now_ns, predictor, host_cost_ns);
            }
        }
    }

    #[must_use]
    pub fn token_belongs_to(&self, actor: &ActorScope, token: RenderToken) -> bool {
        self.token_owners.get(&token) == Some(actor)
    }

    pub fn dispatch_due(&mut self, now_ns: u64) -> Vec<(ActorScope, RenderHint)> {
        let mut due = self
            .actors
            .iter()
            .filter_map(|(actor, state)| match &state.phase {
                Phase::Armed(armed)
                    if now_ns >= armed.plan.dispatch_ns
                        && self
                            .actor_composition_release_ns(actor)
                            .is_none_or(|release_ns| now_ns >= release_ns) =>
                {
                    Some(actor.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if let Some(last) = &self.last_dispatched {
            due.sort_by_key(|actor| (actor <= last, actor.clone()));
        }
        due.truncate(MAX_DISPATCH_PER_TURN);
        let mut hints = Vec::with_capacity(due.len());
        for actor in due {
            let Some(state) = self.actors.get_mut(&actor) else {
                continue;
            };
            let Phase::Armed(armed) = std::mem::replace(&mut state.phase, Phase::Idle) else {
                continue;
            };
            // This callback is the causal cutoff. Actor input admitted after
            // these values are copied belongs to the next opportunity. The
            // runtime's per-actor presentation FIFO orders this hint after
            // every event through the watermark even when the component lane
            // is full, so dispatch does not wait for guest reduction.
            let covered_revision = state.requested_revision;
            let event_watermark = state.admitted_watermark;
            let token = RenderToken(self.next_token);
            let Some(next_token) = self.next_token.checked_add(1) else {
                state.phase = Phase::Armed(armed);
                self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
                break;
            };
            self.next_token = next_token;
            let outstanding = OutstandingOpportunity {
                token,
                plan: armed.plan,
                covered_revision,
                event_watermark,
            };
            state.phase = Phase::Outstanding(outstanding.clone());
            if state.last_dispatch_ns == Some(now_ns) {
                self.metrics.catch_up_bursts = self.metrics.catch_up_bursts.saturating_add(1);
            }
            state.last_dispatch_ns = Some(now_ns);
            state.dispatched_count = state.dispatched_count.saturating_add(1);
            self.token_owners.insert(token, actor.clone());
            self.active_traces.insert(
                token,
                CorrelationTrace {
                    token,
                    actor: actor.clone(),
                    target_ns: outstanding.plan.target_ns,
                    opportunity_end_ns: outstanding.plan.opportunity_end_ns,
                    deadline_ns: outstanding.plan.deadline_ns,
                    dispatched_ns: now_ns,
                    accepted_ns: None,
                    graphics_ready_ns: BTreeMap::new(),
                    composition_ns: None,
                    gpu_submission_ns: None,
                    presentation_ns: None,
                    deadline_met: None,
                    target_presented: None,
                    settlement: None,
                    expected_panes: BTreeSet::new(),
                    input_correlations: {
                        let split =
                            state
                                .pending_inputs
                                .partition_point(|(watermark, _at, _id)| {
                                    *watermark <= outstanding.event_watermark
                                });
                        state.pending_inputs.drain(..split).collect()
                    },
                    measured_opportunity: false,
                    readiness_noted: false,
                    composition_released: false,
                },
            );
            self.metrics.dispatched = self.metrics.dispatched.saturating_add(1);
            self.metrics.deadline_wakeups = self.metrics.deadline_wakeups.saturating_add(1);
            hints.push((
                actor.clone(),
                RenderHint {
                    token,
                    target_presentation_time_ns: outstanding.plan.target_ns,
                    submission_deadline_ns: outstanding.plan.deadline_ns,
                    relevant_panes: state.relevant_panes.iter().copied().collect(),
                    event_watermark: outstanding.event_watermark,
                },
            ));
            self.last_dispatched = Some(actor);
            if let Some(trace) = self.active_traces.get(&token) {
                for (_, _, profile_id) in &trace.input_correlations {
                    step_input_profile(
                        brood_profiler::Kind::FlowStep,
                        "actor_dispatch",
                        *profile_id,
                        token,
                    );
                }
            }
        }
        hints
    }

    pub fn reject(&mut self, token: RenderToken) -> Result<(), FinishError> {
        if !self.token_owners.contains_key(&token) {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return Err(FinishError::InvalidToken);
        }
        self.metrics.rejected = self.metrics.rejected.saturating_add(1);
        Ok(())
    }

    pub fn finish(
        &mut self,
        token: RenderToken,
        now_ns: u64,
        changed_panes: &[u64],
    ) -> Result<Option<AcceptedFrame>, FinishError> {
        self.finish_timed(token, now_ns, now_ns, changed_panes)
    }

    pub fn finish_timed(
        &mut self,
        token: RenderToken,
        guest_finished_ns: u64,
        accepted_ns: u64,
        changed_panes: &[u64],
    ) -> Result<Option<AcceptedFrame>, FinishError> {
        let changed_pane_limit_exceeded = changed_panes.len() > MAX_RELEVANT_PANES;
        let Some(actor) = self.token_owners.remove(&token) else {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return Err(FinishError::InvalidToken);
        };
        let dispatched_ns = self
            .active_traces
            .get(&token)
            .map_or(guest_finished_ns, |trace| trace.dispatched_ns);
        let planned_dispatch_ns = self
            .actors
            .get(&actor)
            .and_then(|state| match &state.phase {
                Phase::Outstanding(outstanding) => Some(outstanding.plan.dispatch_ns),
                _ => None,
            })
            .unwrap_or(dispatched_ns);
        let composition_barrier_delay_ns = self
            .actor_composition_release_ns(&actor)
            .filter(|release_ns| planned_dispatch_ns < *release_ns && dispatched_ns >= *release_ns)
            .map_or(0, |release_ns| {
                release_ns.saturating_sub(planned_dispatch_ns)
            });
        let Some(state) = self.actors.get_mut(&actor) else {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return Err(FinishError::InvalidToken);
        };
        let Phase::Outstanding(outstanding) = std::mem::replace(&mut state.phase, Phase::Idle)
        else {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return Err(FinishError::InvalidToken);
        };
        if outstanding.token != token {
            state.phase = Phase::Outstanding(outstanding);
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return Err(FinishError::InvalidToken);
        }
        state.satisfied_revision = state.satisfied_revision.max(outstanding.covered_revision);
        state.accepted_count = state.accepted_count.saturating_add(1);
        state.abandonments = 0;
        state.backoff_until_ns = 0;
        // Estimate guest execution from the instant the hint was actually
        // dispatched. A pre-armed opportunity can deliberately remain behind
        // the preceding frame's composition barrier after its planned wake.
        // Charging that causal wait to the guest inflates a sub-millisecond
        // actor toward one refresh interval and makes every following hint run
        // immediately after presentation. Deadline feedback separately grows
        // the predictor's safety margin for genuine late event-loop wakes.
        let elapsed = guest_finished_ns.saturating_sub(dispatched_ns);
        state.guest_cost_ns = if elapsed > state.guest_cost_ns {
            elapsed
        } else {
            state
                .guest_cost_ns
                .saturating_mul(7)
                .saturating_add(elapsed)
                .saturating_div(8)
                .max(1)
        };
        // A deliberately retained composition barrier is host scheduling
        // time, not actor deadline budget. Shift only that known delay; any
        // additional event-loop wake lateness still feeds the predictor's
        // safety margin.
        let deadline_met = guest_finished_ns
            <= outstanding
                .plan
                .deadline_ns
                .saturating_add(composition_barrier_delay_ns);
        self.metrics.accepted = self.metrics.accepted.saturating_add(1);
        if !deadline_met {
            self.metrics.late = self.metrics.late.saturating_add(1);
        }
        self.deadline_results.push_back(!deadline_met);
        let measured_opportunity = self.presented_opportunities >= DIAGNOSTIC_WARMUP_OPPORTUNITIES;
        if measured_opportunity {
            self.measured_accepted_opportunities =
                self.measured_accepted_opportunities.saturating_add(1);
        }
        if let Some(trace) = self.active_traces.get_mut(&token) {
            trace.accepted_ns = Some(accepted_ns);
            trace.deadline_met = Some(deadline_met);
            trace.measured_opportunity = measured_opportunity;
        }
        if let Some(trace) = self.active_traces.get(&token) {
            for (_, _, profile_id) in &trace.input_correlations {
                step_input_profile(
                    brood_profiler::Kind::FlowStep,
                    "wasm_frame_accepted",
                    *profile_id,
                    token,
                );
            }
        }
        if changed_pane_limit_exceeded {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            self.settle_trace(token, Settlement::Cancelled);
            return Err(FinishError::InvalidToken);
        }
        let submitted_changes = !changed_panes.is_empty();
        let changed_panes = changed_panes
            .iter()
            .copied()
            .filter(|pane| state.relevant_panes.contains(pane))
            .collect::<BTreeSet<_>>();
        if !submitted_changes {
            self.metrics.empty = self.metrics.empty.saturating_add(1);
            self.settle_trace(token, Settlement::NoChange);
            return Ok(None);
        }
        let revision = self.next_actor_frame_revision;
        let Some(next_revision) = self.next_actor_frame_revision.checked_add(1) else {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            self.settle_trace(token, Settlement::Cancelled);
            return Err(FinishError::InvalidToken);
        };
        self.next_actor_frame_revision = next_revision;
        if changed_panes.is_empty() {
            // The guest legally supplied new state, but every changed pane
            // became hidden before finish. Retain the batch revision in the
            // graphics bridge for later exposure while settling this token as
            // no visible composition work.
            self.settle_trace(token, Settlement::NoChange);
            return Ok(Some(AcceptedFrame {
                token,
                actor_frame_revision: revision,
                deadline_met,
            }));
        }
        if let Some(trace) = self.active_traces.get_mut(&token) {
            trace.expected_panes.extend(changed_panes);
        }
        if let Some(previous) = self.newest_unpresented.insert(actor, token) {
            let previous_inputs = self
                .active_traces
                .get_mut(&previous)
                .map(|trace| std::mem::take(&mut trace.input_correlations))
                .unwrap_or_default();
            if let Some(trace) = self.active_traces.get_mut(&token) {
                Self::merge_correlations(&mut trace.input_correlations, previous_inputs);
            }
            self.metrics.superseded = self.metrics.superseded.saturating_add(1);
            self.settle_trace(previous, Settlement::Superseded);
        }
        Ok(Some(AcceptedFrame {
            token,
            actor_frame_revision: revision,
            deadline_met,
        }))
    }

    pub fn finish_for_actor(
        &mut self,
        actor: &ActorScope,
        token: RenderToken,
        now_ns: u64,
        changed_panes: &[u64],
    ) -> Result<Option<AcceptedFrame>, FinishError> {
        if !self.token_belongs_to(actor, token) {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return Err(FinishError::InvalidToken);
        }
        self.finish(token, now_ns, changed_panes)
    }

    pub fn finish_for_actor_timed(
        &mut self,
        actor: &ActorScope,
        token: RenderToken,
        guest_finished_ns: u64,
        accepted_ns: u64,
        changed_panes: &[u64],
    ) -> Result<Option<AcceptedFrame>, FinishError> {
        if !self.token_belongs_to(actor, token) {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return Err(FinishError::InvalidToken);
        }
        self.finish_timed(token, guest_finished_ns, accepted_ns, changed_panes)
    }

    pub fn abandon(&mut self, actor: &ActorScope, now_ns: u64) -> bool {
        let Some(state) = self.actors.get_mut(actor) else {
            return false;
        };
        let Phase::Outstanding(outstanding) = std::mem::replace(&mut state.phase, Phase::Idle)
        else {
            return false;
        };
        self.token_owners.remove(&outstanding.token);
        let inputs = self
            .active_traces
            .get_mut(&outstanding.token)
            .map(|trace| std::mem::take(&mut trace.input_correlations))
            .unwrap_or_default();
        Self::merge_correlations(&mut state.pending_inputs, inputs);
        state.abandonments = state.abandonments.saturating_add(1).min(7);
        let shift = u32::from(state.abandonments.saturating_sub(1));
        state.backoff_until_ns = now_ns.saturating_add(
            ABANDON_BACKOFF_BASE_NS
                .saturating_mul(1_u64 << shift)
                .min(ABANDON_BACKOFF_MAX_NS),
        );
        self.metrics.abandoned = self.metrics.abandoned.saturating_add(1);
        self.settle_trace(outstanding.token, Settlement::Abandoned);
        true
    }

    pub fn mark_graphics_ready(&mut self, token: RenderToken, pane: u64, at_ns: u64) -> bool {
        let readiness = {
            let Some(trace) = self.active_traces.get_mut(&token) else {
                return false;
            };
            let invalid_transition = trace.settlement.is_some()
                || trace
                    .accepted_ns
                    .is_none_or(|accepted_ns| at_ns < accepted_ns)
                || trace.composition_ns.is_some()
                || !trace.expected_panes.contains(&pane);
            if invalid_transition {
                self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
                return false;
            }
            if let Some(previous_ready_ns) = trace.graphics_ready_ns.get(&pane).copied() {
                if at_ns < previous_ready_ns {
                    self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
                    return false;
                }
                // Recovery replay can repeat an already-observed ready pane.
                // Accept equal or later duplicates idempotently, retaining the
                // first timestamp as the causal readiness boundary.
                return true;
            }
            trace.graphics_ready_ns.insert(pane, at_ns);
            let all_ready = trace
                .expected_panes
                .iter()
                .all(|pane| trace.graphics_ready_ns.contains_key(pane));
            if all_ready && !trace.readiness_noted {
                trace.readiness_noted = true;
                trace.accepted_ns.map(|accepted_ns| {
                    (
                        trace.actor.clone(),
                        at_ns.saturating_sub(accepted_ns),
                        trace
                            .input_correlations
                            .iter()
                            .map(|(_, _, id)| *id)
                            .collect::<Vec<_>>(),
                    )
                })
            } else {
                None
            }
        };
        if let Some((actor, measured_ns, profile_ids)) = readiness
            && let Some(state) = self.actors.get_mut(&actor)
        {
            for profile_id in profile_ids {
                step_input_profile(
                    brood_profiler::Kind::FlowStep,
                    "graphics_ready",
                    profile_id,
                    token,
                );
            }
            let measured_ns = measured_ns.clamp(1, MAX_HOST_COST_NS);
            state.readiness_cost_ns = if measured_ns > state.readiness_cost_ns {
                measured_ns
            } else {
                state
                    .readiness_cost_ns
                    .saturating_mul(7)
                    .saturating_add(measured_ns)
                    .saturating_div(8)
                    .max(1)
            };
        }
        true
    }

    /// Makes each actor-atomic frame eligible for the redraw already scheduled
    /// for its target. Actors do not form a mutual readiness barrier: a slow
    /// peer must not turn the whole target cohort into a 30 Hz pipeline.
    pub fn release_ready_composition(&mut self) -> Vec<RenderToken> {
        let mut released = Vec::new();
        for trace in self
            .active_traces
            .values_mut()
            .filter(|trace| Self::trace_graphics_ready(trace) && !trace.composition_released)
        {
            trace.composition_released = true;
            released.push(trace.token);
        }
        released
    }

    #[must_use]
    pub fn next_redraw_request_deadline_ns(
        &self,
        host_tail_ns: u64,
        redraw_hop_ns: u64,
    ) -> Option<u64> {
        self.pending_redraw_targets()
            .into_iter()
            .filter(|target_ns| *target_ns > self.redraw_requested_through_ns)
            .map(|target_ns| {
                target_ns
                    .saturating_sub(redraw_hop_ns)
                    .saturating_sub(host_tail_ns)
            })
            .min()
    }

    pub fn request_due_redraw(
        &mut self,
        now_ns: u64,
        host_tail_ns: u64,
        redraw_hop_ns: u64,
    ) -> Option<u64> {
        let due_through = self
            .pending_redraw_targets()
            .into_iter()
            .filter(|target_ns| {
                *target_ns > self.redraw_requested_through_ns
                    && now_ns
                        >= target_ns
                            .saturating_sub(redraw_hop_ns)
                            .saturating_sub(host_tail_ns)
            })
            .max();
        let due_through = due_through?;
        self.redraw_requested_through_ns = due_through;
        Some(due_through)
    }

    pub fn mark_redraw_consumed(&mut self) -> u64 {
        self.redraw_consumed_through_ns = self
            .redraw_consumed_through_ns
            .max(self.redraw_requested_through_ns);
        self.redraw_consumed_through_ns
    }

    pub fn reset_redraw_frontier(&mut self) {
        self.redraw_requested_through_ns = 0;
        self.redraw_consumed_through_ns = 0;
        for state in self.actors.values_mut() {
            if matches!(state.phase, Phase::Armed(_)) {
                state.phase = Phase::Idle;
            }
        }
    }

    #[must_use]
    pub fn token_target_redraw_consumed(&self, token: RenderToken) -> bool {
        self.active_traces
            .get(&token)
            .is_some_and(|trace| trace.target_ns <= self.redraw_consumed_through_ns)
    }

    #[must_use]
    pub fn token_target_ns(&self, token: RenderToken) -> Option<u64> {
        self.active_traces.get(&token).map(|trace| trace.target_ns)
    }

    pub fn mark_composed(&mut self, token: RenderToken, at_ns: u64) -> bool {
        let Some(trace) = self.active_traces.get_mut(&token) else {
            return false;
        };
        let ready_at_ns = trace.graphics_ready_ns.values().copied().max().unwrap_or(0);
        if trace.accepted_ns.is_none()
            || !Self::trace_graphics_ready(trace)
            || !trace.composition_released
            || trace.composition_ns.is_some()
            || at_ns < ready_at_ns
        {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return false;
        }
        trace.composition_ns = Some(at_ns);
        for (_, _, profile_id) in &trace.input_correlations {
            step_input_profile(
                brood_profiler::Kind::FlowStep,
                "composed",
                *profile_id,
                token,
            );
        }
        true
    }

    pub fn mark_gpu_submitted(&mut self, token: RenderToken, at_ns: u64) -> bool {
        let Some(trace) = self.active_traces.get_mut(&token) else {
            return false;
        };
        if trace
            .composition_ns
            .is_none_or(|composed_ns| trace.gpu_submission_ns.is_some() || at_ns < composed_ns)
        {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return false;
        }
        trace.gpu_submission_ns = Some(at_ns);
        for (_, _, profile_id) in &trace.input_correlations {
            step_input_profile(
                brood_profiler::Kind::FlowStep,
                "gpu_submitted",
                *profile_id,
                token,
            );
        }
        true
    }

    pub fn mark_presented(&mut self, token: RenderToken, at_ns: u64, estimated: bool) -> bool {
        let Some(trace) = self.active_traces.get_mut(&token) else {
            return false;
        };
        if trace
            .gpu_submission_ns
            .is_none_or(|submitted_ns| trace.presentation_ns.is_some() || at_ns < submitted_ns)
        {
            self.metrics.protocol_errors = self.metrics.protocol_errors.saturating_add(1);
            return false;
        }
        let new_presentation_opportunity = self.last_presentation_ns != Some(at_ns);
        let measured = if new_presentation_opportunity {
            self.presented_opportunities >= DIAGNOSTIC_WARMUP_OPPORTUNITIES
        } else {
            self.current_presentation_measured
        };
        trace.presentation_ns = Some(at_ns);
        if new_presentation_opportunity {
            if let Some(previous) = self.last_presentation_ns {
                if self.presentation_intervals_ns.len() == MAX_INPUT_CORRELATIONS {
                    self.presentation_intervals_ns.pop_front();
                }
                self.presentation_intervals_ns
                    .push_back(at_ns.saturating_sub(previous));
                if measured {
                    if self.measured_presentation_intervals_ns.len() == MAX_INPUT_CORRELATIONS {
                        self.measured_presentation_intervals_ns.pop_front();
                    }
                    self.measured_presentation_intervals_ns
                        .push_back(at_ns.saturating_sub(previous));
                }
            }
            self.last_presentation_ns = Some(at_ns);
            self.current_presentation_measured = measured;
            self.presented_opportunities = self.presented_opportunities.saturating_add(1);
            if self.presented_opportunities == DIAGNOSTIC_WARMUP_OPPORTUNITIES {
                self.warmup_late_baseline = Some(self.metrics.late);
            }
        }
        let target_presented = at_ns
            <= if estimated {
                trace.opportunity_end_ns
            } else {
                trace.target_ns
            };
        trace.target_presented = Some(target_presented);
        if !target_presented {
            self.metrics.intended_target_misses =
                self.metrics.intended_target_misses.saturating_add(1);
        }
        if new_presentation_opportunity && brood_profiler::enabled() {
            brood_profiler::instant(
                "frame",
                "presented",
                Some(token.0),
                vec![("estimated".into(), estimated.into())],
            );
        }
        for (_watermark, started_ns, profile_id) in &trace.input_correlations {
            if self.key_to_present_ns.len() == MAX_INPUT_CORRELATIONS {
                self.key_to_present_ns.pop_front();
            }
            self.key_to_present_ns
                .push_back(at_ns.saturating_sub(*started_ns));
            if trace.measured_opportunity {
                if self.measured_key_to_present_ns.len() == MAX_INPUT_CORRELATIONS {
                    self.measured_key_to_present_ns.pop_front();
                }
                self.measured_key_to_present_ns
                    .push_back(at_ns.saturating_sub(*started_ns));
            }
            self.metrics.key_to_present_samples =
                self.metrics.key_to_present_samples.saturating_add(1);
            step_input_profile(
                brood_profiler::Kind::FlowEnd,
                "presented",
                *profile_id,
                token,
            );
        }
        if self.newest_unpresented.get(&trace.actor) == Some(&token) {
            self.newest_unpresented.remove(&trace.actor);
        }
        if let Some(state) = self.actors.get_mut(&trace.actor) {
            state.presented_count = state.presented_count.saturating_add(1);
        }
        self.settle_trace(token, Settlement::Presented { estimated });
        true
    }

    pub fn cancel_generation(&mut self, actor: &ActorScope) {
        self.cancel_outstanding(actor, Settlement::Cancelled);
        if let Some(token) = self.newest_unpresented.remove(actor) {
            self.metrics.cancelled = self.metrics.cancelled.saturating_add(1);
            self.settle_trace(token, Settlement::Cancelled);
        }
        self.actors.remove(actor);
    }

    pub fn cancel_accepted(&mut self, token: RenderToken) -> bool {
        let Some(actor) = self
            .active_traces
            .get(&token)
            .map(|trace| trace.actor.clone())
        else {
            return false;
        };
        if self.newest_unpresented.get(&actor) == Some(&token) {
            self.newest_unpresented.remove(&actor);
        }
        self.metrics.cancelled = self.metrics.cancelled.saturating_add(1);
        self.settle_trace(token, Settlement::Cancelled);
        true
    }

    #[must_use]
    pub const fn metrics(&self) -> SchedulerMetrics {
        self.metrics
    }

    #[must_use]
    pub const fn completed_traces(&self) -> &VecDeque<CorrelationTrace> {
        &self.completed_traces
    }

    #[must_use]
    pub const fn key_to_present_ns(&self) -> &VecDeque<u64> {
        &self.key_to_present_ns
    }

    #[must_use]
    pub const fn presentation_intervals_ns(&self) -> &VecDeque<u64> {
        &self.presentation_intervals_ns
    }

    #[must_use]
    pub const fn measured_key_to_present_ns(&self) -> &VecDeque<u64> {
        &self.measured_key_to_present_ns
    }

    #[must_use]
    pub const fn measured_presentation_intervals_ns(&self) -> &VecDeque<u64> {
        &self.measured_presentation_intervals_ns
    }

    #[must_use]
    pub fn warmup_opportunities(&self) -> u64 {
        self.presented_opportunities
            .min(DIAGNOSTIC_WARMUP_OPPORTUNITIES)
    }

    #[must_use]
    pub const fn measured_opportunities(&self) -> u64 {
        self.measured_accepted_opportunities
    }

    #[must_use]
    pub fn measured_deadline_misses(&self) -> u64 {
        self.metrics
            .late
            .saturating_sub(self.warmup_late_baseline.unwrap_or(self.metrics.late))
    }

    #[must_use]
    pub const fn measured_target_misses(&self) -> u64 {
        self.measured_target_misses
    }

    pub fn drain_deadline_results(&mut self) -> impl Iterator<Item = bool> + '_ {
        self.deadline_results.drain(..)
    }

    #[must_use]
    pub fn actor_progress(&self) -> Vec<ActorSchedulerProgress> {
        self.actors
            .iter()
            .map(|(actor, state)| ActorSchedulerProgress {
                actor: actor.name.clone(),
                generation: actor.generation,
                dispatched: state.dispatched_count,
                accepted: state.accepted_count,
                presented: state.presented_count,
                host_cost_estimate_ns: state.host_cost_ns,
                readiness_cost_estimate_ns: state.readiness_cost_ns,
            })
            .collect()
    }

    fn accepted_target_ns(&self, actor: &ActorScope) -> Option<u64> {
        self.newest_unpresented
            .get(actor)
            .and_then(|token| self.active_traces.get(token))
            .map(|trace| trace.target_ns)
    }

    fn actor_has_unreleased_frame(&self, actor: &ActorScope) -> bool {
        self.newest_unpresented
            .get(actor)
            .and_then(|token| self.active_traces.get(token))
            .is_some_and(|trace| !trace.composition_released)
    }

    fn actor_composition_release_ns(&self, actor: &ActorScope) -> Option<u64> {
        self.newest_unpresented
            .get(actor)
            .and_then(|token| self.active_traces.get(token))
            .filter(|trace| trace.composition_ns.is_none())
            .map(|trace| trace.target_ns.saturating_add(LATE_COMPOSITION_RELEASE_NS))
    }

    fn trace_graphics_ready(trace: &CorrelationTrace) -> bool {
        trace.accepted_ns.is_some()
            && !trace.expected_panes.is_empty()
            && trace
                .expected_panes
                .iter()
                .all(|pane| trace.graphics_ready_ns.contains_key(pane))
    }

    fn pending_redraw_targets(&self) -> BTreeSet<u64> {
        self.actors
            .values()
            .filter_map(|state| match &state.phase {
                Phase::Armed(armed) => Some(armed.plan.target_ns),
                Phase::Outstanding(outstanding) => Some(outstanding.plan.target_ns),
                Phase::Idle => None,
            })
            .chain(
                self.active_traces
                    .values()
                    .filter(|trace| trace.accepted_ns.is_some() && trace.composition_ns.is_none())
                    .map(|trace| trace.target_ns),
            )
            .collect()
    }

    fn cancel_outstanding(&mut self, actor: &ActorScope, settlement: Settlement) {
        let Some(state) = self.actors.get_mut(actor) else {
            return;
        };
        let previous = std::mem::replace(&mut state.phase, Phase::Idle);
        if let Phase::Outstanding(outstanding) = previous {
            self.token_owners.remove(&outstanding.token);
            self.metrics.cancelled = self.metrics.cancelled.saturating_add(1);
            self.settle_trace(outstanding.token, settlement);
        }
    }

    fn settle_trace(&mut self, token: RenderToken, settlement: Settlement) {
        let Some(mut trace) = self.active_traces.remove(&token) else {
            return;
        };
        let presented = matches!(settlement, Settlement::Presented { .. });
        let missed_target =
            trace.accepted_ns.is_some() && ((!presented) || trace.target_presented == Some(false));
        if missed_target {
            if !presented {
                trace.target_presented = Some(false);
                self.metrics.intended_target_misses =
                    self.metrics.intended_target_misses.saturating_add(1);
            }
            if trace.measured_opportunity {
                self.measured_target_misses = self.measured_target_misses.saturating_add(1);
            }
        }
        trace.settlement = Some(settlement);
        if self.completed_traces.len() == MAX_CORRELATION_TRACES {
            self.completed_traces.pop_front();
        }
        self.completed_traces.push_back(trace);
    }

    fn merge_correlations(
        target: &mut impl CorrelationCollection,
        correlations: impl IntoIterator<Item = (u64, u64, u64)>,
    ) {
        target.extend_correlations(correlations);
    }
}

trait CorrelationCollection {
    fn extend_correlations(&mut self, correlations: impl IntoIterator<Item = (u64, u64, u64)>);
}

impl CorrelationCollection for Vec<(u64, u64, u64)> {
    fn extend_correlations(&mut self, correlations: impl IntoIterator<Item = (u64, u64, u64)>) {
        self.extend(correlations);
        self.sort_unstable();
        self.dedup();
        if self.len() > MAX_INPUT_CORRELATIONS {
            self.drain(..self.len() - MAX_INPUT_CORRELATIONS);
        }
    }
}

impl CorrelationCollection for VecDeque<(u64, u64, u64)> {
    fn extend_correlations(&mut self, correlations: impl IntoIterator<Item = (u64, u64, u64)>) {
        let mut merged = self.drain(..).chain(correlations).collect::<Vec<_>>();
        merged.sort_unstable();
        merged.dedup();
        let retain_from = merged.len().saturating_sub(MAX_INPUT_CORRELATIONS);
        self.extend(merged.drain(retain_from..));
    }
}

/// Retains one coherent pending frame per actor. Visible changed panes promote
/// only when all visible changes are ready; hidden changes never form a
/// barrier and are retained for later exposure.
#[derive(Debug)]
pub struct AtomicActorFrames<T> {
    visible: BTreeMap<u64, T>,
    hidden: BTreeMap<u64, T>,
    pending: BTreeMap<ActorScope, PendingFrame<T>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameStageError {
    LimitOrDuplicate,
}

#[derive(Debug)]
struct PendingFrame<T> {
    revision: u64,
    token: RenderToken,
    changed: BTreeMap<u64, T>,
    waiting_visible: BTreeSet<u64>,
}

impl<T> Default for AtomicActorFrames<T> {
    fn default() -> Self {
        Self {
            visible: BTreeMap::new(),
            hidden: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }
}

impl<T> AtomicActorFrames<T> {
    pub fn stage(
        &mut self,
        actor: ActorScope,
        revision: u64,
        token: RenderToken,
        changes: Vec<(u64, T)>,
        visible_panes: &BTreeSet<u64>,
    ) -> Result<Option<RenderToken>, FrameStageError> {
        if changes.len() > MAX_RELEVANT_PANES
            || (!self.pending.contains_key(&actor) && self.pending.len() >= MAX_RENDER_ACTORS)
            || self
                .visible
                .len()
                .saturating_add(self.hidden.len())
                .saturating_add(changes.len())
                > MAX_RETAINED_FRAME_PANES
        {
            return Err(FrameStageError::LimitOrDuplicate);
        }
        let mut changed = BTreeMap::new();
        for (pane, value) in changes {
            if changed.insert(pane, value).is_some() {
                return Err(FrameStageError::LimitOrDuplicate);
            }
        }
        let waiting_visible = changed
            .keys()
            .filter(|pane| visible_panes.contains(pane))
            .copied()
            .collect();
        let superseded = self.pending.insert(
            actor,
            PendingFrame {
                revision,
                token,
                changed,
                waiting_visible,
            },
        );
        Ok(superseded.map(|frame| frame.token))
    }

    pub fn mark_ready(&mut self, actor: &ActorScope, revision: u64, pane: u64) -> bool {
        let Some(frame) = self.pending.get_mut(actor) else {
            return false;
        };
        if frame.revision != revision || !frame.changed.contains_key(&pane) {
            return false;
        }
        frame.waiting_visible.remove(&pane)
    }

    pub fn promote_ready(
        &mut self,
        actor: &ActorScope,
        visible_panes: &BTreeSet<u64>,
    ) -> Option<RenderToken> {
        let ready = self
            .pending
            .get(actor)
            .is_some_and(|frame| frame.waiting_visible.is_empty());
        if !ready {
            return None;
        }
        let frame = self.pending.remove(actor)?;
        for (pane, value) in frame.changed {
            if visible_panes.contains(&pane) {
                self.visible.insert(pane, value);
            } else {
                self.hidden.insert(pane, value);
            }
        }
        Some(frame.token)
    }

    pub fn expose(&mut self, pane: u64) -> bool {
        let Some(value) = self.hidden.remove(&pane) else {
            return false;
        };
        self.visible.insert(pane, value);
        true
    }

    #[must_use]
    pub fn visible(&self, pane: u64) -> Option<&T> {
        self.visible.get(&pane)
    }

    pub fn remove_actor_panes(&mut self, actor: &ActorScope, panes: &BTreeSet<u64>) {
        self.pending.remove(actor);
        self.visible.retain(|pane, _| !panes.contains(pane));
        self.hidden.retain(|pane, _| !panes.contains(pane));
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::struct_field_names,
    reason = "deterministic scheduler tests fail immediately when a required opportunity is absent"
)]
mod tests {
    use super::*;

    const HZ_60: u64 = 16_666_667;
    const HZ_120: u64 = 8_333_333;

    fn ready_scheduler() -> (RenderScheduler, ActorScope, FramePredictor) {
        let actor = ActorScope::new("clock", 7);
        let mut scheduler = RenderScheduler::default();
        assert!(scheduler.register(actor.clone()));
        assert!(scheduler.set_relevant_panes(&actor, [11, 12]));
        (scheduler, actor, FramePredictor::new(HZ_60))
    }

    #[test]
    fn configured_rate_with_skipped_cells_does_not_become_vrr() {
        let mut predictor = FramePredictor::new(HZ_60);
        let mut presented = 0;
        predictor.observe_presented(presented);
        for interval in [
            HZ_60 + 500_000,
            HZ_60 * 2 - 2_000_000,
            HZ_60 - 750_000,
            HZ_60 * 2 + 1_000_000,
        ]
        .into_iter()
        .cycle()
        .take(12)
        {
            presented = presented.saturating_add(interval);
            predictor.observe_presented(presented);
        }
        assert_eq!(predictor.predicted_interval_ns(), HZ_60);
    }

    #[test]
    fn alternating_120_and_60_hz_remains_vrr() {
        let mut predictor = FramePredictor::new(HZ_60);
        let mut presented = 0;
        predictor.observe_presented(presented);
        for interval in [HZ_120, HZ_60, HZ_120] {
            presented = presented.saturating_add(interval);
            predictor.observe_presented(presented);
        }
        assert_eq!(predictor.predicted_interval_ns(), HZ_60);
    }

    #[test]
    fn alternating_120_and_60_hz_remains_vrr_with_120_hz_fallback() {
        let mut predictor = FramePredictor::new(HZ_120);
        let mut presented = 0;
        predictor.observe_presented(presented);
        for interval in [HZ_120, HZ_60, HZ_120] {
            presented = presented.saturating_add(interval);
            predictor.observe_presented(presented);
        }
        assert_eq!(predictor.predicted_interval_ns(), HZ_60);
    }

    #[test]
    fn predictor_reset_replaces_stale_120_hz_history_with_60_hz_fallback() {
        let mut predictor = FramePredictor::new(HZ_120);
        for presented in [0, HZ_120, HZ_120 * 2, HZ_120 * 3] {
            predictor.observe_presented(presented);
        }
        assert_eq!(predictor.predicted_interval_ns(), HZ_120);

        predictor.reset(Some(HZ_60));

        assert_eq!(predictor.predicted_interval_ns(), HZ_60);
        assert_eq!(predictor.last_presented_ns, None);
        assert!(predictor.intervals_ns.is_empty());
        assert_eq!(predictor.plan(0, 1, 1).target_ns, HZ_60);
    }

    #[test]
    fn long_presentation_gap_discards_stale_history_and_recovers_fallback() {
        let mut predictor = FramePredictor::new(HZ_60);
        for presented in [0, HZ_120, HZ_120 * 2, HZ_120 * 3] {
            predictor.observe_presented(presented);
        }
        assert_eq!(predictor.predicted_interval_ns(), HZ_120);

        let after_gap = HZ_120 * 3 + MAX_INTERVAL_NS + 1;
        predictor.observe_presented(after_gap);

        assert!(predictor.intervals_ns.is_empty());
        assert_eq!(predictor.predicted_interval_ns(), HZ_60);
        assert_eq!(predictor.plan(after_gap, 1, 1).target_ns, after_gap + HZ_60);
    }

    #[test]
    fn same_refresh_recovery_reanchors_without_destroying_trained_cadence() {
        let mut predictor = FramePredictor::new(HZ_60);
        for presented in (0..=8).map(|cell| cell * HZ_120) {
            predictor.observe_presented(presented);
        }
        assert_eq!(predictor.predicted_interval_ns(), HZ_120);
        let trained_history = predictor.intervals_ns.clone();

        predictor.reanchor_next_presentation();
        let recovered_at = HZ_120 * 8 + 79_500_000;
        predictor.observe_presented(recovered_at);

        assert_eq!(predictor.predicted_interval_ns(), HZ_120);
        assert_eq!(predictor.intervals_ns, trained_history);
        assert_eq!(predictor.last_presented_ns, Some(recovered_at));

        predictor.observe_presented(recovered_at + HZ_120);
        assert_eq!(predictor.predicted_interval_ns(), HZ_120);

        // A real refresh change remains a reset, not a recovery re-anchor.
        predictor.reanchor_next_presentation();
        predictor.reset(Some(HZ_60));
        assert_eq!(predictor.predicted_interval_ns(), HZ_60);
        assert!(predictor.intervals_ns.is_empty());
        assert!(!predictor.reanchor_next_present);
    }

    fn dispatch_one(
        scheduler: &mut RenderScheduler,
        actor: &ActorScope,
        predictor: &FramePredictor,
        now: u64,
    ) -> RenderHint {
        assert!(scheduler.arm(actor, now, predictor, 1_000_000));
        let deadline = scheduler.next_deadline_ns().unwrap();
        let hints = scheduler.dispatch_due(deadline);
        assert_eq!(hints.len(), 1);
        hints.into_iter().next().unwrap().1
    }

    fn release_and_compose(scheduler: &mut RenderScheduler, token: RenderToken, at_ns: u64) {
        let expected_panes = scheduler
            .active_traces
            .get(&token)
            .unwrap()
            .expected_panes
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for pane in expected_panes {
            if !scheduler
                .active_traces
                .get(&token)
                .unwrap()
                .graphics_ready_ns
                .contains_key(&pane)
            {
                assert!(scheduler.mark_graphics_ready(token, pane, at_ns.saturating_sub(1)));
            }
        }
        assert!(scheduler.release_ready_composition().contains(&token));
        assert!(scheduler.mark_composed(token, at_ns));
    }

    fn submit_and_present(
        scheduler: &mut RenderScheduler,
        token: RenderToken,
        at_ns: u64,
        estimated: bool,
    ) {
        assert!(scheduler.mark_gpu_submitted(token, at_ns.saturating_sub(1)));
        assert!(scheduler.mark_presented(token, at_ns, estimated));
    }

    #[test]
    fn same_timestamp_multi_actor_frames_cross_warmup_as_one_opportunity() {
        let mut scheduler = RenderScheduler::default();
        let actors = [
            (ActorScope::new("warmup-a", 1), 1_u64),
            (ActorScope::new("warmup-b", 1), 2_u64),
            (ActorScope::new("warmup-c", 1), 3_u64),
        ];
        for (actor, pane) in &actors {
            assert!(scheduler.register(actor.clone()));
            assert!(scheduler.set_relevant_panes(actor, [*pane]));
            assert!(scheduler.set_subscription(actor, true));
        }
        let mut predictor = FramePredictor::new(HZ_60);
        predictor.observe_presented(0);
        let mut presented_ns = 0;
        for opportunity in 1..=61_u64 {
            scheduler.arm_all(presented_ns, &predictor, 1_000_000);
            for (actor, _) in &actors {
                let watermark = scheduler.admit_event(actor, false).unwrap();
                assert!(scheduler.note_key_input(actor, watermark, presented_ns.saturating_add(1)));
            }
            let deadline = scheduler.next_deadline_ns().unwrap();
            let hints = scheduler.dispatch_due(deadline);
            assert_eq!(hints.len(), actors.len());
            let target_ns = hints.first().unwrap().1.target_presentation_time_ns;
            assert!(
                hints
                    .iter()
                    .all(|(_, hint)| hint.target_presentation_time_ns == target_ns)
            );
            for (actor, hint) in &hints {
                let pane = actors
                    .iter()
                    .find_map(|(candidate, pane)| (candidate == actor).then_some(*pane))
                    .unwrap();
                scheduler
                    .finish(hint.token, deadline.saturating_add(100_000), &[pane])
                    .unwrap();
                assert!(scheduler.mark_graphics_ready(
                    hint.token,
                    pane,
                    deadline.saturating_add(200_000)
                ));
            }
            let released = scheduler.release_ready_composition();
            assert_eq!(released.len(), actors.len());
            let mut lagging = None;
            for (actor, hint) in &hints {
                assert!(scheduler.mark_composed(hint.token, target_ns.saturating_sub(500_000)));
                if opportunity == DIAGNOSTIC_WARMUP_OPPORTUNITIES && actor.name == "warmup-c" {
                    assert!(scheduler.mark_gpu_submitted(hint.token, target_ns.saturating_sub(1)));
                    lagging = Some(hint.token);
                } else {
                    submit_and_present(&mut scheduler, hint.token, target_ns, true);
                }
            }
            if let Some(lagging) = lagging {
                // This trace was accepted before the 60th presented frame but
                // lands just after it. Its key and target result remain in the
                // warm-up set, matching the measured-opportunity denominator.
                assert!(scheduler.mark_presented(lagging, target_ns.saturating_add(1), true));
            }
            predictor.observe_presented(target_ns);
            presented_ns = target_ns;

            if opportunity == DIAGNOSTIC_WARMUP_OPPORTUNITIES {
                assert_eq!(scheduler.warmup_opportunities(), opportunity);
                assert_eq!(scheduler.measured_opportunities(), 0);
                assert!(scheduler.measured_key_to_present_ns().is_empty());
            }
        }
        assert_eq!(scheduler.warmup_opportunities(), 60);
        assert_eq!(
            scheduler.measured_opportunities(),
            u64::try_from(actors.len()).unwrap()
        );
        assert_eq!(scheduler.measured_key_to_present_ns().len(), actors.len());
    }

    #[test]
    fn request_coalesces_without_losing_request_during_turn() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        assert!(scheduler.request_render(&actor));
        let first = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        assert!(scheduler.request_render(&actor));
        assert!(
            scheduler
                .finish(first.token, first.submission_deadline_ns, &[])
                .unwrap()
                .is_none()
        );
        assert!(scheduler.arm(
            &actor,
            first.target_presentation_time_ns,
            &predictor,
            1_000_000
        ));
        assert!(scheduler.next_deadline_ns().is_some());
    }

    #[test]
    fn revision_and_watermark_exhaustion_fail_without_aliasing() {
        let (mut scheduler, actor, _predictor) = ready_scheduler();
        let state = scheduler.actors.get_mut(&actor).unwrap();
        state.requested_revision = u64::MAX;
        assert!(!scheduler.request_render(&actor));
        assert_eq!(
            scheduler
                .actors
                .get(&actor)
                .map(|state| state.requested_revision),
            Some(u64::MAX)
        );
        let relevant_before = scheduler.actors.get(&actor).unwrap().relevant_panes.clone();
        assert!(!scheduler.set_relevant_panes(&actor, [11]));
        assert_eq!(
            scheduler
                .actors
                .get(&actor)
                .map(|state| &state.relevant_panes),
            Some(&relevant_before)
        );

        let state = scheduler.actors.get_mut(&actor).unwrap();
        state.requested_revision = 0;
        state.admitted_watermark = u64::MAX;
        assert_eq!(scheduler.admit_event(&actor, true), None);
        assert_eq!(
            scheduler
                .actors
                .get(&actor)
                .map(|state| state.admitted_watermark),
            Some(u64::MAX)
        );
        assert_eq!(
            scheduler
                .actors
                .get(&actor)
                .map(|state| state.requested_revision),
            Some(0)
        );
        assert_eq!(scheduler.metrics().protocol_errors, 3);
    }

    #[test]
    fn actor_host_cost_estimate_rises_immediately_and_decays_boundedly() {
        let (mut scheduler, actor, _predictor) = ready_scheduler();
        assert!(scheduler.note_actor_host_cost(&actor, 10_000_000));
        let raised = scheduler
            .actor_progress()
            .into_iter()
            .find(|progress| progress.actor == actor.name)
            .unwrap()
            .host_cost_estimate_ns;
        assert_eq!(raised, 10_000_000);
        assert!(scheduler.note_actor_host_cost(&actor, 1_000_000));
        let decayed = scheduler
            .actor_progress()
            .into_iter()
            .find(|progress| progress.actor == actor.name)
            .unwrap()
            .host_cost_estimate_ns;
        assert_eq!(decayed, 8_875_000);
    }

    #[test]
    fn composition_barrier_delay_is_not_charged_as_guest_execution() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        predictor.observe_presented(0);
        assert!(scheduler.set_subscription(&actor, true));
        assert!(scheduler.arm(&actor, 0, &predictor, 1_000_000));

        let planned_dispatch_ns = scheduler.next_deadline_ns().unwrap();
        let actual_dispatch_ns = planned_dispatch_ns.saturating_add(12_000_000);
        let hint = scheduler.dispatch_due(actual_dispatch_ns).pop().unwrap().1;
        scheduler
            .finish(
                hint.token,
                actual_dispatch_ns.saturating_add(300_000),
                &[11],
            )
            .unwrap();

        assert_eq!(
            scheduler
                .actors
                .get(&actor)
                .map(|state| state.guest_cost_ns),
            Some(1_787_500),
            "the intentional barrier wait must not inflate the guest estimate"
        );
    }

    #[test]
    fn delayed_graphics_readiness_moves_the_next_latest_safe_dispatch_earlier() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        predictor.observe_presented(0);
        assert!(scheduler.set_subscription(&actor, true));
        let first = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let accepted_ns = scheduler
            .active_traces
            .get(&first.token)
            .unwrap()
            .dispatched_ns
            .saturating_add(100_000);
        scheduler.finish(first.token, accepted_ns, &[11]).unwrap();
        let graphics_ready_ns = accepted_ns.saturating_add(4_000_000);
        assert!(scheduler.mark_graphics_ready(first.token, 11, graphics_ready_ns));
        assert_eq!(
            scheduler
                .actor_progress()
                .into_iter()
                .find(|progress| progress.actor == actor.name)
                .unwrap()
                .readiness_cost_estimate_ns,
            4_000_000
        );
        release_and_compose(
            &mut scheduler,
            first.token,
            graphics_ready_ns.saturating_add(1),
        );
        submit_and_present(
            &mut scheduler,
            first.token,
            graphics_ready_ns.saturating_add(3),
            true,
        );
        predictor.observe_presented(graphics_ready_ns.saturating_add(3));

        let baseline_dispatch = {
            let state = scheduler.actors.get(&actor).unwrap();
            predictor
                .plan(
                    first.target_presentation_time_ns,
                    state.guest_cost_ns,
                    state.host_cost_ns.max(1_000_000),
                )
                .dispatch_ns
        };
        assert!(scheduler.arm(
            &actor,
            first.target_presentation_time_ns,
            &predictor,
            1_000_000
        ));
        assert!(scheduler.next_deadline_ns().unwrap() < baseline_dispatch);
    }

    #[test]
    fn validation_readiness_and_global_tail_are_partitioned_and_summed_once() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        predictor.observe_presented(0);
        assert!(scheduler.set_subscription(&actor, true));
        let hint = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let Some(dispatched_ns) = scheduler
            .active_traces
            .get(&hint.token)
            .map(|trace| trace.dispatched_ns)
        else {
            assert!(
                scheduler.active_traces.contains_key(&hint.token),
                "dispatched hint must have an active trace"
            );
            return;
        };
        let guest_finished_ns = dispatched_ns.saturating_add(1_000_000);
        let validation_finished_ns = guest_finished_ns.saturating_add(3_000_000);
        scheduler
            .finish_for_actor_timed(
                &actor,
                hint.token,
                guest_finished_ns,
                validation_finished_ns,
                &[11],
            )
            .unwrap();
        assert!(scheduler.note_actor_host_cost(&actor, 3_000_000));
        let graphics_ready_ns = validation_finished_ns.saturating_add(5_000_000);
        assert!(scheduler.mark_graphics_ready(hint.token, 11, graphics_ready_ns));
        assert_eq!(scheduler.release_ready_composition(), [hint.token]);
        let composed_ns = graphics_ready_ns.saturating_add(1);
        assert!(scheduler.mark_composed(hint.token, composed_ns));
        let presented_ns = hint
            .target_presentation_time_ns
            .max(composed_ns.saturating_add(1));
        submit_and_present(&mut scheduler, hint.token, presented_ns, true);

        let state = scheduler.actors.get(&actor).unwrap();
        assert_eq!(state.host_cost_ns, 3_000_000);
        assert_eq!(state.readiness_cost_ns, 5_000_000);
        let global_tail_ns = 2_000_000;
        let expected = predictor.plan(
            presented_ns,
            state.guest_cost_ns,
            3_000_000 + 5_000_000 + global_tail_ns,
        );
        assert!(scheduler.arm(&actor, presented_ns, &predictor, global_tail_ns,));
        assert_eq!(scheduler.next_deadline_ns(), Some(expected.dispatch_ns));
    }

    #[test]
    fn same_target_ready_frames_join_prearmed_redraw_without_peer_barrier() {
        let mut predictor = FramePredictor::new(HZ_60);
        let mut scheduler = RenderScheduler::default();
        let actors = [
            ActorScope::new("cohort-a", 1),
            ActorScope::new("cohort-b", 1),
            ActorScope::new("cohort-c", 1),
            ActorScope::new("cohort-d", 1),
        ];
        for (index, actor) in actors.iter().enumerate() {
            assert!(scheduler.register(actor.clone()));
            assert!(scheduler.set_relevant_panes(actor, [index as u64 + 1]));
            assert!(scheduler.set_subscription(actor, true));
        }
        scheduler.arm_all(0, &predictor, 1_000_000);
        let redraw_hop_ns = 14_000_000;
        let redraw_deadline = scheduler
            .next_redraw_request_deadline_ns(1_000_000, redraw_hop_ns)
            .unwrap();
        assert_eq!(
            scheduler.request_due_redraw(redraw_deadline - 1, 1_000_000, redraw_hop_ns),
            None
        );
        let dispatch_ns = scheduler.next_deadline_ns().unwrap();
        assert!(redraw_deadline < dispatch_ns);
        assert_eq!(
            scheduler.request_due_redraw(redraw_deadline, 1_000_000, redraw_hop_ns),
            Some(HZ_60)
        );
        let hints = scheduler.dispatch_due(dispatch_ns);
        assert_eq!(hints.len(), 4);
        let target_ns = hints.first().unwrap().1.target_presentation_time_ns;
        assert!(
            hints
                .iter()
                .all(|(_actor, hint)| { hint.target_presentation_time_ns == target_ns })
        );
        for ((actor, hint), pane) in hints.iter().zip([1, 2, 3, 4]) {
            assert_eq!(scheduler.token_owners.get(&hint.token), Some(actor));
            scheduler
                .finish(hint.token, dispatch_ns + 100_000, &[pane])
                .unwrap();
        }

        for ((_, hint), pane) in hints.iter().take(3).zip([1, 2, 3]) {
            assert!(scheduler.mark_graphics_ready(hint.token, pane, dispatch_ns + 200_000));
            assert_eq!(scheduler.release_ready_composition(), vec![hint.token]);
        }
        assert_eq!(scheduler.mark_redraw_consumed(), target_ns);
        for (_actor, hint) in hints.iter().take(3) {
            assert!(scheduler.mark_composed(hint.token, target_ns - 500_000));
            assert!(scheduler.mark_gpu_submitted(hint.token, target_ns - 250_000));
            assert!(scheduler.mark_presented(hint.token, target_ns, true));
        }
        predictor.observe_presented(target_ns);
        scheduler.arm_all(target_ns, &predictor, 1_000_000);
        let next_dispatch = scheduler.next_deadline_ns().unwrap();
        let next_hints = scheduler.dispatch_due(next_dispatch);
        assert_eq!(next_hints.len(), 3);
        assert!(next_hints.iter().all(|(actor, hint)| {
            actor != actors.get(3).unwrap() && hint.target_presentation_time_ns == target_ns + HZ_60
        }));

        assert!(scheduler.mark_graphics_ready(
            hints.get(3).unwrap().1.token,
            4,
            dispatch_ns + 300_000
        ));
        let late = hints.get(3).unwrap().1.token;
        assert_eq!(scheduler.release_ready_composition(), vec![late]);
        assert!(scheduler.token_target_redraw_consumed(late));
    }

    #[test]
    fn redraw_is_prearmed_by_positive_hop_and_host_tail_before_latest_hint() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));
        scheduler.arm_all(0, &predictor, 1_000_000);
        let redraw_hop_ns = 14_000_000;
        let redraw_deadline = scheduler
            .next_redraw_request_deadline_ns(1_000_000, redraw_hop_ns)
            .unwrap();
        let dispatch_deadline = scheduler.next_deadline_ns().unwrap();
        assert_eq!(
            redraw_deadline,
            HZ_60.saturating_sub(1_000_000 + redraw_hop_ns)
        );
        assert!(redraw_deadline < dispatch_deadline);
        assert_eq!(
            scheduler.request_due_redraw(redraw_deadline, 1_000_000, redraw_hop_ns),
            Some(HZ_60)
        );
    }

    #[test]
    fn steady_subscription_prearms_redraw_but_keeps_one_cell_cadence() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));
        predictor.observe_presented(0);
        let host_tail_ns = 1_000_000;
        let redraw_hop_ns = 14_000_000;
        let mut presented_ns = 0;
        for opportunity in 1..=120_u64 {
            scheduler.arm_all(presented_ns, &predictor, host_tail_ns);
            let redraw_deadline = scheduler
                .next_redraw_request_deadline_ns(host_tail_ns, redraw_hop_ns)
                .unwrap();
            let dispatch_deadline = scheduler.next_deadline_ns().unwrap();
            assert!(redraw_deadline < dispatch_deadline);
            let target_ns = opportunity.saturating_mul(HZ_60);
            assert_eq!(
                scheduler.request_due_redraw(redraw_deadline, host_tail_ns, redraw_hop_ns),
                Some(target_ns)
            );
            let hint = scheduler.dispatch_due(dispatch_deadline).pop().unwrap().1;
            assert_eq!(hint.target_presentation_time_ns, target_ns);
            scheduler
                .finish(hint.token, dispatch_deadline + 100_000, &[11])
                .unwrap();
            assert!(scheduler.mark_graphics_ready(hint.token, 11, dispatch_deadline + 200_000));
            assert_eq!(scheduler.release_ready_composition(), vec![hint.token]);
            assert_eq!(scheduler.mark_redraw_consumed(), target_ns);
            assert!(scheduler.mark_composed(hint.token, target_ns - host_tail_ns));
            assert!(scheduler.mark_gpu_submitted(hint.token, target_ns - 500_000));
            assert!(scheduler.mark_presented(hint.token, target_ns, true));
            predictor.observe_presented(target_ns);
            presented_ns = target_ns;
        }
        assert_eq!(scheduler.metrics().superseded, 0);
        assert_eq!(scheduler.metrics().intended_target_misses, 0);
    }

    #[test]
    fn causal_key_before_latest_hint_is_included_in_the_prearmed_target() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        predictor.observe_presented(0);
        assert!(scheduler.set_subscription(&actor, true));
        scheduler.arm_all(0, &predictor, 1_000_000);
        let redraw_deadline = scheduler
            .next_redraw_request_deadline_ns(1_000_000, 14_000_000)
            .unwrap();
        assert_eq!(
            scheduler.request_due_redraw(redraw_deadline, 1_000_000, 14_000_000),
            Some(HZ_60)
        );
        let initial_dispatch = scheduler.next_deadline_ns().unwrap();
        let key_at = initial_dispatch - 100_000;
        let watermark = scheduler.admit_event(&actor, false).unwrap();
        assert!(scheduler.note_key_input(&actor, watermark, key_at));
        assert!(scheduler.mark_reduced_through(&actor, watermark));
        scheduler.arm_all(key_at, &predictor, 1_000_000);
        let dispatch = scheduler.next_deadline_ns().unwrap();
        let hint = scheduler.dispatch_due(dispatch).pop().unwrap().1;
        assert_eq!(hint.target_presentation_time_ns, HZ_60);
        assert_eq!(hint.event_watermark, watermark);
        scheduler
            .finish(hint.token, dispatch + 100_000, &[11])
            .unwrap();
        assert!(scheduler.mark_graphics_ready(hint.token, 11, dispatch + 200_000));
        assert_eq!(scheduler.release_ready_composition(), vec![hint.token]);
        assert_eq!(scheduler.mark_redraw_consumed(), HZ_60);
        assert!(scheduler.mark_composed(hint.token, HZ_60 - 500_000));
        assert!(scheduler.mark_gpu_submitted(hint.token, HZ_60 - 250_000));
        assert!(scheduler.mark_presented(hint.token, HZ_60, true));
        assert_eq!(
            scheduler.key_to_present_ns().front(),
            Some(&(HZ_60 - key_at))
        );
    }

    #[test]
    fn renderer_recovery_rearms_unconsumed_target_without_stale_frontier() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));
        scheduler.arm_all(0, &predictor, 1_000_000);
        let redraw_deadline = scheduler
            .next_redraw_request_deadline_ns(1_000_000, 1_000_000)
            .unwrap();
        assert_eq!(
            scheduler.request_due_redraw(redraw_deadline, 1_000_000, 1_000_000),
            Some(HZ_60)
        );
        assert_eq!(scheduler.mark_redraw_consumed(), HZ_60);
        scheduler.reset_redraw_frontier();
        scheduler.arm_all(0, &predictor, 1_000_000);
        assert_eq!(
            scheduler.next_redraw_request_deadline_ns(1_000_000, 1_000_000),
            Some(HZ_60 - 2_000_000)
        );
        assert_eq!(
            scheduler.request_due_redraw(redraw_deadline, 1_000_000, 1_000_000),
            Some(HZ_60)
        );
    }

    #[test]
    fn empty_prearmed_callback_does_not_settle_a_later_ready_token() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));
        scheduler.arm_all(0, &predictor, 1_000_000);
        let redraw_deadline = scheduler
            .next_redraw_request_deadline_ns(1_000_000, 14_000_000)
            .unwrap();
        assert_eq!(
            scheduler.request_due_redraw(redraw_deadline, 1_000_000, 14_000_000),
            Some(HZ_60)
        );
        assert_eq!(scheduler.mark_redraw_consumed(), HZ_60);
        assert_eq!(scheduler.metrics().intended_target_misses, 0);
        assert!(scheduler.completed_traces().is_empty());

        let dispatch = scheduler.next_deadline_ns().unwrap();
        let hint = scheduler.dispatch_due(dispatch).pop().unwrap().1;
        scheduler
            .finish(hint.token, dispatch + 100_000, &[11])
            .unwrap();
        assert!(scheduler.mark_graphics_ready(hint.token, 11, dispatch + 200_000));
        assert_eq!(scheduler.release_ready_composition(), vec![hint.token]);
        assert!(scheduler.token_target_redraw_consumed(hint.token));
        assert!(scheduler.active_traces.contains_key(&hint.token));
        assert_eq!(scheduler.metrics().intended_target_misses, 0);
    }

    #[test]
    fn subscription_and_separate_request_have_independent_state() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));
        let hint = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        assert!(scheduler.request_render(&actor));
        assert!(scheduler.set_subscription(&actor, false));
        scheduler
            .finish(hint.token, hint.submission_deadline_ns, &[])
            .unwrap();
        assert!(scheduler.arm(
            &actor,
            hint.target_presentation_time_ns,
            &predictor,
            1_000_000
        ));
    }

    #[test]
    fn disabling_subscription_retires_its_armed_deadline_immediately() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));
        let initial = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        scheduler
            .finish(initial.token, initial.submission_deadline_ns, &[])
            .unwrap();
        assert!(scheduler.arm(
            &actor,
            initial.target_presentation_time_ns,
            &predictor,
            1_000_000,
        ));
        assert!(scheduler.next_deadline_ns().is_some());

        assert!(scheduler.set_subscription(&actor, false));
        assert_eq!(scheduler.next_deadline_ns(), None);
        assert!(scheduler.dispatch_due(u64::MAX).is_empty());
    }

    #[test]
    fn disabling_subscription_preserves_an_armed_explicit_request() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));
        let initial = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        scheduler
            .finish(initial.token, initial.submission_deadline_ns, &[])
            .unwrap();
        assert!(scheduler.request_render(&actor));
        assert!(scheduler.arm(
            &actor,
            initial.target_presentation_time_ns,
            &predictor,
            1_000_000,
        ));

        assert!(scheduler.set_subscription(&actor, false));
        let deadline = scheduler.next_deadline_ns().unwrap();
        assert_eq!(scheduler.dispatch_due(deadline).len(), 1);
    }

    #[test]
    fn deadline_dispatch_freezes_cutoff_without_waiting_for_reduction() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        let watermark = scheduler.admit_event(&actor, true).unwrap();
        assert!(scheduler.arm(&actor, 0, &predictor, 1_000_000));
        let deadline = scheduler.next_deadline_ns().unwrap();
        let hint = scheduler.dispatch_due(deadline).pop().unwrap().1;
        assert_eq!(hint.event_watermark, watermark);
        assert!(scheduler.mark_reduced_through(&actor, watermark));

        let later = scheduler.admit_event(&actor, true).unwrap();
        assert!(later > hint.event_watermark);
        let outstanding = scheduler.actors.get(&actor).unwrap();
        assert!(matches!(
            &outstanding.phase,
            Phase::Outstanding(opportunity)
                if opportunity.event_watermark == watermark
                    && opportunity.covered_revision < outstanding.requested_revision
        ));
    }

    #[test]
    fn continuous_one_millisecond_input_cannot_starve_due_dispatch() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        scheduler.set_subscription(&actor, true);
        assert!(scheduler.arm(&actor, 0, &predictor, 1_000_000));
        let deadline = scheduler.next_deadline_ns().unwrap();
        let mut dispatched = Vec::new();
        for key in 0..260_u64 {
            let at_ns = key.saturating_mul(1_000_000);
            let watermark = scheduler.admit_event(&actor, false).unwrap();
            assert!(scheduler.note_key_input(&actor, watermark, at_ns));
            assert!(scheduler.mark_reduced_through(&actor, watermark.saturating_sub(5)));
            scheduler.arm_all(at_ns, &predictor, 1_000_000);
            for dispatched_hint in scheduler.dispatch_due(at_ns) {
                scheduler
                    .finish(dispatched_hint.1.token, at_ns, &[])
                    .unwrap();
                dispatched.push(dispatched_hint);
            }
        }

        assert!(dispatched.len() > 10);
        let hint = &dispatched.first().unwrap().1;
        assert!(hint.event_watermark < 260);
        assert!(hint.event_watermark > 0);
        assert!(deadline <= hint.event_watermark.saturating_mul(1_000_000));
        assert!(
            scheduler.actors.get(&actor).unwrap().admitted_watermark > hint.event_watermark,
            "input admitted after the deadline cutoff must remain for the next opportunity"
        );
        assert!(dispatched.windows(2).all(
            |pair| matches!(pair, [left, right] if left.1.event_watermark < right.1.event_watermark)
        ));
    }

    #[test]
    fn one_outstanding_token_and_armed_retarget_keep_fixed_dispatch() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        scheduler.set_subscription(&actor, true);
        let first = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let fixed_target = first.target_presentation_time_ns;
        scheduler.request_render(&actor);
        assert!(scheduler.dispatch_due(u64::MAX).is_empty());
        assert!(scheduler.token_belongs_to(&actor, first.token));
        let trace = scheduler.active_traces.get(&first.token).unwrap();
        assert_eq!(trace.target_ns, fixed_target);
        scheduler
            .finish(first.token, first.submission_deadline_ns, &[])
            .unwrap();
        assert!(scheduler.arm(&actor, fixed_target, &predictor, 1_000_000));
        let watermark = scheduler.admit_event(&actor, false).unwrap();
        assert!(scheduler.mark_reduced_through(&actor, watermark));
        assert!(scheduler.retarget_armed(&actor, fixed_target, &predictor, 1_000_000));
        assert_eq!(scheduler.dispatch_due(u64::MAX).len(), 1);
    }

    #[test]
    fn accepted_unpresented_target_advances_subscription_and_dirty_work_one_cell() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        predictor.observe_presented(0);
        assert!(scheduler.set_subscription(&actor, true));

        let first = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let first_dispatch = scheduler
            .active_traces
            .get(&first.token)
            .unwrap()
            .dispatched_ns;
        let accepted_at = first_dispatch.saturating_add(100_000);
        scheduler.finish(first.token, accepted_at, &[11]).unwrap();

        // A distinct dirty revision waits while the accepted frame has not
        // reached composition. This prevents redraw backlog from replacing
        // the frame before it can be sampled by Winit.
        assert!(scheduler.request_render(&actor));
        assert!(!scheduler.arm(&actor, accepted_at, &predictor, 1_000_000));
        release_and_compose(
            &mut scheduler,
            first.token,
            accepted_at.saturating_add(50_000),
        );

        // Composition, rather than presentation feedback, releases the actor.
        // The queued dirty revision and subscription then target the next cell.
        assert!(scheduler.arm(&actor, accepted_at, &predictor, 1_000_000));
        let second_dispatch = scheduler.next_deadline_ns().unwrap();
        assert!(second_dispatch > first.target_presentation_time_ns);

        // Confirmation can arrive while the later cell is armed; it does not
        // cancel or postpone that work.
        submit_and_present(
            &mut scheduler,
            first.token,
            first.target_presentation_time_ns,
            true,
        );
        let second = scheduler.dispatch_due(second_dispatch).pop().unwrap().1;
        assert_eq!(
            second.target_presentation_time_ns,
            first.target_presentation_time_ns.saturating_add(HZ_60)
        );
        scheduler
            .finish(second.token, second.submission_deadline_ns, &[11])
            .unwrap();
        assert_eq!(scheduler.metrics().superseded, 0);
        assert_eq!(scheduler.metrics().intended_target_misses, 0);
    }

    #[test]
    fn released_frame_exposes_next_cell_before_stale_composition() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        predictor.observe_presented(0);
        assert!(scheduler.set_subscription(&actor, true));

        let first = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let accepted_at = first.submission_deadline_ns.saturating_sub(1_000_000);
        scheduler.finish(first.token, accepted_at, &[11]).unwrap();
        assert!(scheduler.mark_graphics_ready(
            first.token,
            11,
            accepted_at.saturating_add(100_000)
        ));
        assert_eq!(scheduler.release_ready_composition(), [first.token]);
        assert_eq!(
            scheduler.request_due_redraw(u64::MAX, 1_000_000, 14_000_000),
            Some(first.target_presentation_time_ns)
        );
        assert_eq!(
            scheduler.mark_redraw_consumed(),
            first.target_presentation_time_ns
        );

        assert!(scheduler.arm(&actor, accepted_at, &predictor, 1_000_000));
        let Some(fixed_plan) = scheduler
            .actors
            .get(&actor)
            .and_then(|state| match &state.phase {
                Phase::Armed(armed) => Some(armed.plan),
                _ => None,
            })
        else {
            assert!(
                scheduler
                    .actors
                    .get(&actor)
                    .is_some_and(|state| matches!(state.phase, Phase::Armed(_))),
                "expected prearmed opportunity"
            );
            return;
        };
        assert_eq!(scheduler.next_deadline_ns(), Some(fixed_plan.dispatch_ns));

        // Continuous admitted input cannot postpone the fixed cutoff. Once a
        // frame is graphics-ready, holding the next hint behind its eventual
        // composition can make a frame which is already one cell late keep
        // every later frame one cell late as well.
        for input in 1..=260_u64 {
            let at_ns = accepted_at.saturating_add(input.saturating_mul(1_000));
            assert!(scheduler.admit_event(&actor, false).is_some());
            scheduler.arm_all(at_ns, &predictor, 1_000_000);
            assert_eq!(scheduler.next_deadline_ns(), Some(fixed_plan.dispatch_ns));
            assert!(scheduler.dispatch_due(at_ns).is_empty());
        }
        let Some(retained_plan) =
            scheduler
                .actors
                .get(&actor)
                .and_then(|state| match &state.phase {
                    Phase::Armed(armed) => Some(armed.plan),
                    _ => None,
                })
        else {
            assert!(
                scheduler
                    .actors
                    .get(&actor)
                    .is_some_and(|state| matches!(state.phase, Phase::Armed(_))),
                "expected retained armed opportunity"
            );
            return;
        };
        assert_eq!(retained_plan, fixed_plan);

        let second = scheduler
            .dispatch_due(fixed_plan.dispatch_ns)
            .pop()
            .unwrap()
            .1;
        assert_eq!(
            second.target_presentation_time_ns,
            first.target_presentation_time_ns.saturating_add(HZ_60)
        );
        scheduler
            .finish(second.token, second.submission_deadline_ns, &[11])
            .unwrap();
        assert_eq!(scheduler.metrics().superseded, 1);
        assert!(scheduler.completed_traces().iter().any(|trace| {
            trace.token == first.token && trace.settlement == Some(Settlement::Superseded)
        }));
    }

    #[test]
    fn one_frame_late_feedback_converges_without_repeating_target_cell() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        for presented in [0, HZ_60, HZ_60 * 2, HZ_60 * 3] {
            predictor.observe_presented(presented);
        }
        assert!(scheduler.set_subscription(&actor, true));

        let first = dispatch_one(&mut scheduler, &actor, &predictor, HZ_60 * 3);
        let first_dispatch = scheduler
            .active_traces
            .get(&first.token)
            .unwrap()
            .dispatched_ns;
        let first_accepted = first_dispatch.saturating_add(100_000);
        scheduler
            .finish(first.token, first_accepted, &[11])
            .unwrap();
        release_and_compose(
            &mut scheduler,
            first.token,
            first_accepted.saturating_add(50_000),
        );

        let second = dispatch_one(&mut scheduler, &actor, &predictor, first_accepted);
        assert_eq!(
            second.target_presentation_time_ns,
            first.target_presentation_time_ns.saturating_add(HZ_60)
        );
        scheduler
            .finish(second.token, second.submission_deadline_ns, &[11])
            .unwrap();
        release_and_compose(
            &mut scheduler,
            second.token,
            second.submission_deadline_ns.saturating_add(2),
        );

        // The first accepted frame really did miss: keep that supersession in
        // the metric. The second frame owns the one-frame-late presentation,
        // so the same delayed feedback does not manufacture another miss.
        assert!(!scheduler.mark_presented(first.token, second.target_presentation_time_ns, true));
        submit_and_present(
            &mut scheduler,
            second.token,
            second.target_presentation_time_ns,
            true,
        );
        assert_eq!(scheduler.metrics().superseded, 1);
        assert_eq!(scheduler.metrics().intended_target_misses, 1);

        predictor.observe_presented(second.target_presentation_time_ns);
        let third = dispatch_one(
            &mut scheduler,
            &actor,
            &predictor,
            second.target_presentation_time_ns,
        );
        assert_eq!(
            third.target_presentation_time_ns,
            second.target_presentation_time_ns.saturating_add(HZ_60)
        );
    }

    #[test]
    fn production_arm_all_retargets_before_due_but_never_chases_a_late_wake() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));
        scheduler.arm_all(0, &predictor, 1_000_000);
        let original = scheduler.next_deadline_ns().unwrap();

        // A larger measured host tail moves dispatch earlier while there is
        // still time to use the same presentation target.
        scheduler.arm_all(1, &predictor, 2_000_000);
        let retargeted = scheduler.next_deadline_ns().unwrap();
        assert!(retargeted < original);

        // Service after the deadline must dispatch the existing opportunity,
        // rather than replanning it one refresh interval into the future.
        scheduler.arm_all(retargeted.saturating_add(1), &predictor, 2_000_000);
        let hints = scheduler.dispatch_due(retargeted.saturating_add(1));
        assert_eq!(hints.len(), 1);
        assert_eq!(hints.first().map(|(scope, _hint)| scope), Some(&actor));
    }

    #[test]
    fn rejection_keeps_token_and_abandonment_backs_off() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        scheduler.request_render(&actor);
        let hint = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        assert_eq!(scheduler.reject(hint.token), Ok(()));
        assert!(scheduler.abandon(&actor, hint.submission_deadline_ns));
        assert!(!scheduler.arm(&actor, hint.submission_deadline_ns, &predictor, 1_000_000));
        assert!(scheduler.arm(
            &actor,
            hint.submission_deadline_ns + ABANDON_BACKOFF_BASE_NS,
            &predictor,
            1_000_000
        ));
    }

    #[test]
    fn rejected_empty_and_abandoned_turns_preserve_contract() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        scheduler.request_render(&actor);
        let rejected = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        assert_eq!(scheduler.reject(rejected.token), Ok(()));
        assert!(scheduler.token_belongs_to(&actor, rejected.token));
        assert_eq!(
            scheduler.finish(rejected.token, rejected.submission_deadline_ns, &[]),
            Ok(None)
        );

        scheduler.request_render(&actor);
        let abandoned = dispatch_one(
            &mut scheduler,
            &actor,
            &predictor,
            rejected.target_presentation_time_ns,
        );
        assert!(scheduler.abandon(&actor, abandoned.submission_deadline_ns));
        assert!(!scheduler.token_belongs_to(&actor, abandoned.token));
        assert_eq!(
            scheduler.finish(abandoned.token, u64::MAX, &[11]),
            Err(FinishError::InvalidToken)
        );
        assert_eq!(scheduler.metrics().empty, 1);
        assert_eq!(scheduler.metrics().abandoned, 1);
    }

    #[test]
    fn key_correlation_survives_abandonment_and_retry_once() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        let watermark = scheduler.admit_event(&actor, true).unwrap();
        assert!(scheduler.note_key_input(&actor, watermark, 10));
        assert!(scheduler.mark_reduced_through(&actor, watermark));
        let first = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        assert!(scheduler.abandon(&actor, first.submission_deadline_ns));

        let retry_at = first.submission_deadline_ns + ABANDON_BACKOFF_BASE_NS;
        let second = dispatch_one(&mut scheduler, &actor, &predictor, retry_at);
        scheduler
            .finish(second.token, second.submission_deadline_ns, &[11])
            .unwrap();
        release_and_compose(
            &mut scheduler,
            second.token,
            second.target_presentation_time_ns.saturating_sub(2),
        );
        submit_and_present(
            &mut scheduler,
            second.token,
            second.target_presentation_time_ns,
            true,
        );
        assert_eq!(scheduler.key_to_present_ns().len(), 1);
        assert_eq!(scheduler.metrics().key_to_present_samples, 1);
    }

    #[test]
    fn key_correlation_transfers_to_newer_accepted_frame_once() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        scheduler.set_subscription(&actor, true);
        let watermark = scheduler.admit_event(&actor, false).unwrap();
        assert!(scheduler.note_key_input(&actor, watermark, 10));
        assert!(scheduler.mark_reduced_through(&actor, watermark));
        let first = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        scheduler
            .finish(first.token, first.submission_deadline_ns, &[11])
            .unwrap();
        release_and_compose(
            &mut scheduler,
            first.token,
            first.submission_deadline_ns.saturating_add(1),
        );

        let second = dispatch_one(
            &mut scheduler,
            &actor,
            &predictor,
            first.target_presentation_time_ns,
        );
        scheduler
            .finish(second.token, second.submission_deadline_ns, &[11])
            .unwrap();
        release_and_compose(
            &mut scheduler,
            second.token,
            second.target_presentation_time_ns.saturating_sub(2),
        );
        submit_and_present(
            &mut scheduler,
            second.token,
            second.target_presentation_time_ns,
            true,
        );
        assert_eq!(scheduler.key_to_present_ns().len(), 1);
        assert_eq!(scheduler.metrics().key_to_present_samples, 1);
    }

    #[test]
    fn late_batch_is_accepted_and_newer_frame_supersedes_old() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        scheduler.set_subscription(&actor, true);
        let first = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let accepted = scheduler
            .finish(first.token, first.submission_deadline_ns + 1, &[11, 12])
            .unwrap()
            .unwrap();
        assert!(!accepted.deadline_met);
        release_and_compose(
            &mut scheduler,
            first.token,
            first.submission_deadline_ns.saturating_add(2),
        );
        let second = dispatch_one(
            &mut scheduler,
            &actor,
            &predictor,
            first.target_presentation_time_ns,
        );
        scheduler
            .finish(second.token, second.submission_deadline_ns, &[11])
            .unwrap();
        assert_eq!(scheduler.metrics().superseded, 1);
        assert!(
            scheduler
                .completed_traces()
                .iter()
                .any(|trace| trace.token == first.token
                    && trace.settlement == Some(Settlement::Superseded))
        );
    }

    #[test]
    fn measured_accepted_frames_settled_without_presentation_are_target_misses() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        scheduler.set_subscription(&actor, true);
        let mut now = 0_u64;
        for _ in 0..DIAGNOSTIC_WARMUP_OPPORTUNITIES {
            let hint = dispatch_one(&mut scheduler, &actor, &predictor, now);
            scheduler
                .finish(hint.token, hint.submission_deadline_ns, &[11])
                .unwrap();
            release_and_compose(
                &mut scheduler,
                hint.token,
                hint.submission_deadline_ns.saturating_add(1),
            );
            submit_and_present(
                &mut scheduler,
                hint.token,
                hint.target_presentation_time_ns,
                true,
            );
            predictor.observe_presented(hint.target_presentation_time_ns);
            now = hint.target_presentation_time_ns;
        }

        let superseded = dispatch_one(&mut scheduler, &actor, &predictor, now);
        scheduler
            .finish(superseded.token, superseded.submission_deadline_ns, &[11])
            .unwrap();
        release_and_compose(
            &mut scheduler,
            superseded.token,
            superseded.submission_deadline_ns.saturating_add(1),
        );
        now = now.saturating_add(HZ_60);
        let cancelled = dispatch_one(&mut scheduler, &actor, &predictor, now);
        scheduler
            .finish(cancelled.token, cancelled.submission_deadline_ns, &[11])
            .unwrap();
        assert!(scheduler.cancel_accepted(cancelled.token));

        assert_eq!(scheduler.measured_opportunities(), 2);
        assert_eq!(scheduler.measured_target_misses(), 2);
        assert_eq!(scheduler.metrics().superseded, 1);
        assert!(scheduler.completed_traces().iter().any(|trace| {
            trace.token == superseded.token && trace.settlement == Some(Settlement::Superseded)
        }));
        assert!(scheduler.completed_traces().iter().any(|trace| {
            trace.token == cancelled.token && trace.settlement == Some(Settlement::Cancelled)
        }));
    }

    #[test]
    fn correlation_records_every_render_stage() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        scheduler.request_render(&actor);
        let hint = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        scheduler
            .finish(hint.token, hint.submission_deadline_ns, &[11, 12])
            .unwrap();
        assert!(scheduler.mark_graphics_ready(hint.token, 11, hint.submission_deadline_ns + 1));
        assert!(scheduler.mark_graphics_ready(hint.token, 12, hint.submission_deadline_ns + 2));
        assert_eq!(scheduler.release_ready_composition(), vec![hint.token]);
        assert!(scheduler.mark_composed(hint.token, hint.submission_deadline_ns + 3));
        assert!(scheduler.mark_gpu_submitted(hint.token, hint.submission_deadline_ns + 4));
        assert!(scheduler.mark_presented(hint.token, hint.target_presentation_time_ns, true));
        let trace = scheduler.completed_traces().back().unwrap();
        assert_eq!(trace.accepted_ns, Some(hint.submission_deadline_ns));
        assert_eq!(trace.graphics_ready_ns.len(), 2);
        assert_eq!(trace.composition_ns, Some(hint.submission_deadline_ns + 3));
        assert_eq!(
            trace.gpu_submission_ns,
            Some(hint.submission_deadline_ns + 4)
        );
        assert_eq!(
            trace.presentation_ns,
            Some(hint.target_presentation_time_ns)
        );
        assert_eq!(
            trace.settlement,
            Some(Settlement::Presented { estimated: true })
        );
    }

    #[test]
    fn correlation_stages_reject_out_of_order_transitions() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        scheduler.request_render(&actor);
        let hint = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let base_errors = scheduler.metrics().protocol_errors;

        assert!(!scheduler.mark_composed(hint.token, hint.submission_deadline_ns));
        scheduler
            .finish(hint.token, hint.submission_deadline_ns, &[11, 12])
            .unwrap();
        assert!(!scheduler.mark_composed(hint.token, hint.submission_deadline_ns + 1));
        assert!(scheduler.mark_graphics_ready(hint.token, 11, hint.submission_deadline_ns + 2));
        assert!(scheduler.mark_graphics_ready(hint.token, 12, hint.submission_deadline_ns + 3));
        assert!(!scheduler.mark_composed(hint.token, hint.submission_deadline_ns + 4));

        assert_eq!(scheduler.release_ready_composition(), vec![hint.token]);
        assert!(scheduler.mark_composed(hint.token, hint.submission_deadline_ns + 5));
        assert!(!scheduler.mark_presented(hint.token, hint.target_presentation_time_ns, true));
        assert!(scheduler.mark_gpu_submitted(hint.token, hint.submission_deadline_ns + 6));
        assert!(scheduler.mark_presented(hint.token, hint.target_presentation_time_ns, true));
        assert_eq!(scheduler.metrics().protocol_errors, base_errors + 4);
    }

    #[test]
    fn graphics_readiness_rejects_missing_acceptance_and_pre_accept_timestamp() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        let hint = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let base_errors = scheduler.metrics().protocol_errors;

        assert!(!scheduler.mark_graphics_ready(hint.token, 11, hint.submission_deadline_ns));
        assert!(
            scheduler
                .active_traces
                .get(&hint.token)
                .unwrap()
                .graphics_ready_ns
                .is_empty()
        );
        scheduler
            .finish(hint.token, hint.submission_deadline_ns, &[11])
            .unwrap();
        assert!(!scheduler.mark_graphics_ready(
            hint.token,
            11,
            hint.submission_deadline_ns.saturating_sub(1)
        ));

        let trace = scheduler.active_traces.get(&hint.token).unwrap();
        assert!(trace.graphics_ready_ns.is_empty());
        assert!(trace.presentation_ns.is_none());
        assert!(trace.settlement.is_none());
        assert_eq!(scheduler.metrics().protocol_errors, base_errors + 2);
    }

    #[test]
    fn graphics_readiness_rejects_backward_duplicate_without_mutation() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        let hint = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let accepted_ns = hint.submission_deadline_ns;
        scheduler.finish(hint.token, accepted_ns, &[11]).unwrap();
        let first_ready_ns = accepted_ns.saturating_add(5);
        assert!(scheduler.mark_graphics_ready(hint.token, 11, first_ready_ns));
        let base_errors = scheduler.metrics().protocol_errors;

        assert!(scheduler.mark_graphics_ready(hint.token, 11, first_ready_ns));
        assert!(scheduler.mark_graphics_ready(hint.token, 11, first_ready_ns.saturating_add(1)));
        assert_eq!(scheduler.metrics().protocol_errors, base_errors);

        assert!(!scheduler.mark_graphics_ready(hint.token, 11, first_ready_ns.saturating_sub(1)));

        let trace = scheduler.active_traces.get(&hint.token).unwrap();
        assert_eq!(trace.graphics_ready_ns.get(&11), Some(&first_ready_ns));
        assert!(trace.presentation_ns.is_none());
        assert!(trace.settlement.is_none());
        assert_eq!(scheduler.metrics().protocol_errors, base_errors + 1);
    }

    #[test]
    fn graphics_readiness_rejects_post_compose_without_false_presentation() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        let hint = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let accepted_ns = hint.submission_deadline_ns;
        scheduler.finish(hint.token, accepted_ns, &[11]).unwrap();
        let ready_ns = accepted_ns.saturating_add(1);
        assert!(scheduler.mark_graphics_ready(hint.token, 11, ready_ns));
        assert_eq!(scheduler.release_ready_composition(), vec![hint.token]);
        let composed_ns = ready_ns.saturating_add(1);
        assert!(scheduler.mark_composed(hint.token, composed_ns));
        let base_errors = scheduler.metrics().protocol_errors;

        assert!(!scheduler.mark_graphics_ready(hint.token, 11, composed_ns.saturating_add(1)));
        assert!(!scheduler.mark_presented(hint.token, hint.target_presentation_time_ns, true));

        let trace = scheduler.active_traces.get(&hint.token).unwrap();
        assert_eq!(trace.graphics_ready_ns.get(&11), Some(&ready_ns));
        assert!(trace.presentation_ns.is_none());
        assert!(trace.settlement.is_none());
        assert!(!scheduler.completed_traces().iter().any(|trace| {
            trace.token == hint.token
                && matches!(trace.settlement, Some(Settlement::Presented { .. }))
        }));
        assert_eq!(scheduler.metrics().protocol_errors, base_errors + 2);
    }

    #[test]
    fn visibility_change_releases_visible_batch_and_retains_hidden_membership_for_next_frame() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));
        let split = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        scheduler
            .finish(split.token, split.submission_deadline_ns, &[11, 12])
            .unwrap();
        assert!(scheduler.mark_graphics_ready(
            split.token,
            11,
            split.submission_deadline_ns.saturating_add(1)
        ));

        assert!(scheduler.set_relevant_panes(&actor, [11]));
        assert_eq!(scheduler.metrics().cancelled, 0);
        assert_eq!(scheduler.release_ready_composition(), vec![split.token]);
        assert!(
            scheduler.mark_composed(split.token, split.submission_deadline_ns.saturating_add(2))
        );
        assert!(
            scheduler
                .mark_gpu_submitted(split.token, split.submission_deadline_ns.saturating_add(3))
        );
        assert!(scheduler.mark_presented(split.token, split.target_presentation_time_ns, true));
        assert!(!scheduler.mark_graphics_ready(split.token, 12, split.target_presentation_time_ns));

        let replacement = dispatch_one(
            &mut scheduler,
            &actor,
            &predictor,
            split.target_presentation_time_ns,
        );
        scheduler
            .finish(
                replacement.token,
                replacement.submission_deadline_ns,
                &[11, 12],
            )
            .unwrap();
        let replacement_trace = scheduler.active_traces.get(&replacement.token).unwrap();
        assert_eq!(replacement_trace.expected_panes, BTreeSet::from([11]));
        release_and_compose(
            &mut scheduler,
            replacement.token,
            replacement.submission_deadline_ns.saturating_add(2),
        );
    }

    #[test]
    fn all_hidden_before_finish_retains_batch_revision_without_composition_barrier() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        let hint = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        assert!(scheduler.set_relevant_panes(&actor, []));
        assert!(scheduler.token_belongs_to(&actor, hint.token));

        let accepted = scheduler
            .finish(hint.token, hint.submission_deadline_ns, &[11, 12])
            .unwrap()
            .unwrap();

        assert_eq!(accepted.token, hint.token);
        assert_eq!(scheduler.metrics().empty, 0);
        assert!(!scheduler.active_traces.contains_key(&hint.token));
        assert!(scheduler.completed_traces().iter().any(|trace| {
            trace.token == hint.token && trace.settlement == Some(Settlement::NoChange)
        }));
        assert!(scheduler.set_relevant_panes(&actor, [11]));
        assert!(scheduler.arm(
            &actor,
            hint.target_presentation_time_ns,
            &predictor,
            1_000_000
        ));
    }

    #[test]
    fn hidden_actor_gets_one_explicit_bootstrap_without_subscription_churn() {
        let actor = ActorScope::new("hidden-bootstrap", 1);
        let predictor = FramePredictor::new(HZ_60);
        let mut scheduler = RenderScheduler::default();
        assert!(scheduler.register(actor.clone()));
        assert!(scheduler.set_subscription(&actor, true));
        assert!(scheduler.arm(&actor, 0, &predictor, 1_000_000));
        let dispatch_ns = scheduler.next_deadline_ns().unwrap();
        let hint = scheduler.dispatch_due(dispatch_ns).pop().unwrap().1;
        let accepted = scheduler
            .finish(hint.token, hint.submission_deadline_ns, &[77])
            .unwrap();

        assert!(accepted.is_some(), "the hidden scene must be retained");
        assert!(scheduler.completed_traces().iter().any(|trace| {
            trace.token == hint.token && trace.settlement == Some(Settlement::NoChange)
        }));
        assert!(
            !scheduler.arm(
                &actor,
                hint.target_presentation_time_ns,
                &predictor,
                1_000_000
            ),
            "subscription alone must not pace an actor with no relevant panes"
        );
    }

    #[derive(Clone, Copy)]
    struct InvalidTokenObservations {
        duplicate_tokens_accepted: usize,
        foreign_tokens_accepted: usize,
        reused_tokens_accepted: usize,
        out_of_order_tokens_accepted: usize,
        attempts: usize,
    }

    fn observe_invalid_tokens() -> InvalidTokenObservations {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        scheduler.request_render(&actor);
        let hint = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        let foreign = ActorScope::new("foreign", actor.generation);
        let foreign_tokens_accepted = usize::from(
            scheduler
                .finish_for_actor(&foreign, hint.token, hint.submission_deadline_ns, &[])
                .is_ok(),
        );
        let out_of_order_tokens_accepted = usize::from(
            scheduler
                .finish_for_actor(
                    &actor,
                    RenderToken(hint.token.0 + 1),
                    hint.submission_deadline_ns,
                    &[],
                )
                .is_ok(),
        );
        scheduler
            .finish_for_actor(&actor, hint.token, hint.submission_deadline_ns, &[])
            .unwrap();
        let duplicate_tokens_accepted = usize::from(
            scheduler
                .finish_for_actor(&actor, hint.token, hint.submission_deadline_ns, &[])
                .is_ok(),
        );
        scheduler.request_render(&actor);
        let next = dispatch_one(
            &mut scheduler,
            &actor,
            &predictor,
            hint.target_presentation_time_ns,
        );
        scheduler
            .finish_for_actor(&actor, next.token, next.submission_deadline_ns, &[])
            .unwrap();
        let reused_tokens_accepted = usize::from(
            scheduler
                .finish_for_actor(&actor, hint.token, hint.submission_deadline_ns, &[])
                .is_ok(),
        );
        InvalidTokenObservations {
            duplicate_tokens_accepted,
            foreign_tokens_accepted,
            reused_tokens_accepted,
            out_of_order_tokens_accepted,
            attempts: 4,
        }
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct DeterministicObservations {
        request_revision_checks: usize,
        lost_request_revisions: usize,
        armed_dispatch_checks: usize,
        stale_armed_dispatches: usize,
        latest_safe_dispatch_checks: usize,
        latest_safe_dispatch_errors: usize,
        infeasible_target_step_checks: usize,
        infeasible_target_step_errors: usize,
        catch_up_burst_checks: usize,
        catch_up_bursts: usize,
    }

    fn observe_infeasible_target_step(observations: &mut DeterministicObservations) {
        let mut predictor = FramePredictor::new(HZ_60);
        predictor.observe_presented(0);
        let plan = predictor.plan(0, 12_000_000, 6_000_000);
        observations.infeasible_target_step_checks += 1;
        observations.infeasible_target_step_errors +=
            usize::from(plan.target_ns != HZ_60.saturating_mul(2));
    }

    fn observe_background_progress() -> usize {
        let mut scheduler = RenderScheduler::default();
        let predictor = FramePredictor::new(HZ_60);
        for index in 0..100 {
            let actor = ActorScope::new(format!("probe-background-{index:03}"), 1);
            assert!(scheduler.register(actor.clone()));
            assert!(scheduler.set_relevant_panes(&actor, [index + 1]));
            assert!(scheduler.set_subscription(&actor, true));
        }
        scheduler.arm_all(0, &predictor, 1_000_000);
        let deadline = scheduler.next_deadline_ns().unwrap();
        let mut actors = scheduler
            .dispatch_due(deadline)
            .into_iter()
            .map(|(actor, _hint)| actor)
            .collect::<BTreeSet<_>>();
        actors.extend(
            scheduler
                .dispatch_due(deadline)
                .into_iter()
                .map(|(actor, _hint)| actor),
        );
        usize::from(actors.len() == 100)
    }

    fn emit_deterministic(
        case: &str,
        opportunities: usize,
        mut observations: DeterministicObservations,
    ) {
        let invalid = observe_invalid_tokens();
        let background_progress = observe_background_progress();
        observe_infeasible_target_step(&mut observations);
        eprintln!(
            "render scheduling deterministic {{\"case\":\"{case}\",\"opportunities\":{opportunities},\"invalid_token_attempts\":{},\"duplicate_tokens_accepted\":{},\"foreign_tokens_accepted\":{},\"reused_tokens_accepted\":{},\"out_of_order_tokens_accepted\":{},\"request_revision_checks\":{},\"lost_request_revisions\":{},\"armed_dispatch_checks\":{},\"stale_armed_dispatches\":{},\"latest_safe_dispatch_checks\":{},\"latest_safe_dispatch_errors\":{},\"infeasible_target_step_checks\":{},\"infeasible_target_step_errors\":{},\"catch_up_burst_checks\":{},\"catch_up_bursts\":{},\"background_progress\":{background_progress}}}",
            invalid.attempts,
            invalid.duplicate_tokens_accepted,
            invalid.foreign_tokens_accepted,
            invalid.reused_tokens_accepted,
            invalid.out_of_order_tokens_accepted,
            observations.request_revision_checks,
            observations.lost_request_revisions,
            observations.armed_dispatch_checks,
            observations.stale_armed_dispatches,
            observations.latest_safe_dispatch_checks,
            observations.latest_safe_dispatch_errors,
            observations.infeasible_target_step_checks,
            observations.infeasible_target_step_errors,
            observations.catch_up_burst_checks,
            observations.catch_up_bursts,
        );
    }

    fn exercise_cadence(case: &str, intervals: impl Iterator<Item = u64>, opportunities: usize) {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        scheduler.set_subscription(&actor, true);
        let mut presented = 0;
        let mut last_token = 0;
        let mut observations = DeterministicObservations::default();
        let mut pending_requested_revision = None;
        for (opportunity, interval) in intervals.take(opportunities).enumerate() {
            predictor.observe_presented(presented);
            let expected = {
                let state = scheduler.actors.get(&actor).unwrap();
                predictor.plan(
                    presented,
                    state.guest_cost_ns,
                    state
                        .host_cost_ns
                        .saturating_add(1_000_000)
                        .saturating_add(state.readiness_cost_ns),
                )
            };
            assert!(scheduler.arm(&actor, presented, &predictor, 1_000_000));
            let dispatch = scheduler.next_deadline_ns().unwrap();
            let mut hints = scheduler.dispatch_due(dispatch);
            observations.catch_up_burst_checks += 1;
            observations.catch_up_bursts += usize::from(hints.len() != 1);
            let hint = hints.pop().unwrap().1;
            observations.catch_up_bursts += scheduler.dispatch_due(dispatch).len();
            assert_eq!(hint.token.0, last_token + 1);
            observations.latest_safe_dispatch_checks += 1;
            observations.latest_safe_dispatch_errors += usize::from(
                dispatch != expected.dispatch_ns
                    || hint.target_presentation_time_ns != expected.target_ns
                    || hint.submission_deadline_ns != expected.deadline_ns,
            );
            let state = scheduler.actors.get(&actor).unwrap();
            observations.armed_dispatch_checks += 1;
            observations.stale_armed_dispatches += match &state.phase {
                Phase::Outstanding(outstanding) => usize::from(
                    outstanding.covered_revision != state.requested_revision
                        || hint.event_watermark != state.admitted_watermark,
                ),
                Phase::Idle | Phase::Armed(_) => 1,
            };
            if let Some(requested_revision) = pending_requested_revision.take() {
                observations.request_revision_checks += 1;
                observations.lost_request_revisions += match &state.phase {
                    Phase::Outstanding(outstanding) => {
                        usize::from(outstanding.covered_revision < requested_revision)
                    }
                    Phase::Idle | Phase::Armed(_) => 1,
                };
            }
            if opportunity + 1 < opportunities && opportunity.is_multiple_of(7) {
                assert!(scheduler.request_render(&actor));
                pending_requested_revision =
                    Some(scheduler.actors.get(&actor).unwrap().requested_revision);
            }
            scheduler
                .finish(hint.token, dispatch + 2_000_000, &[])
                .unwrap();
            last_token = hint.token.0;
            presented = presented.saturating_add(interval);
        }
        let metrics = scheduler.metrics();
        let state = scheduler.actors.get(&actor).unwrap();
        observations.lost_request_revisions += usize::from(pending_requested_revision.is_some());
        observations.lost_request_revisions +=
            usize::from(state.requested_revision != state.satisfied_revision);
        assert_eq!(metrics.dispatched, opportunities as u64);
        assert_eq!(metrics.protocol_errors, 0);
        emit_deterministic(case, opportunities, observations);
    }

    #[test]
    fn virtual_clock_fixed_60_hz_10k() {
        exercise_cadence("fixed_60hz", std::iter::repeat(HZ_60), 10_000);
    }

    #[test]
    fn virtual_clock_fixed_120_hz_10k() {
        exercise_cadence("fixed_120hz", std::iter::repeat(HZ_120), 10_000);
    }

    #[test]
    fn virtual_clock_alternating_vrr_10k() {
        exercise_cadence(
            "alternating_vrr",
            [HZ_120, HZ_60].into_iter().cycle(),
            10_000,
        );
    }

    #[test]
    fn virtual_clock_cost_retarget_and_miss_10k() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        scheduler.set_subscription(&actor, true);
        let mut presented = 0_u64;
        let mut observations = DeterministicObservations::default();
        let mut pending_requested_revision = None;
        for opportunity in 0..10_000_u64 {
            predictor.observe_presented(presented);
            assert!(scheduler.arm(&actor, presented, &predictor, 1_000_000));
            let stale_deadline = scheduler.next_deadline_ns().unwrap();
            let mut plan_now = presented;
            if opportunity.is_multiple_of(3) {
                let watermark = scheduler.admit_event(&actor, false).unwrap();
                scheduler.mark_reduced_through(&actor, watermark);
                plan_now = presented.saturating_add(250_000);
                assert!(plan_now < stale_deadline);
                assert!(scheduler.dispatch_due(plan_now).is_empty());
                scheduler.retarget_armed(&actor, plan_now, &predictor, 1_000_000);
            }
            let expected = {
                let state = scheduler.actors.get(&actor).unwrap();
                predictor.plan(
                    plan_now,
                    state.guest_cost_ns,
                    state
                        .host_cost_ns
                        .saturating_add(1_000_000)
                        .saturating_add(state.readiness_cost_ns),
                )
            };
            let dispatch = scheduler.next_deadline_ns().unwrap();
            let mut hints = scheduler.dispatch_due(dispatch);
            observations.catch_up_burst_checks += 1;
            observations.catch_up_bursts += usize::from(hints.len() != 1);
            let hint = hints.pop().unwrap().1;
            observations.catch_up_bursts += scheduler.dispatch_due(dispatch).len();
            observations.latest_safe_dispatch_checks += 1;
            observations.latest_safe_dispatch_errors += usize::from(
                dispatch != expected.dispatch_ns
                    || hint.target_presentation_time_ns != expected.target_ns
                    || hint.submission_deadline_ns != expected.deadline_ns,
            );
            let state = scheduler.actors.get(&actor).unwrap();
            observations.armed_dispatch_checks += 1;
            observations.stale_armed_dispatches += match &state.phase {
                Phase::Outstanding(outstanding) => usize::from(
                    outstanding.covered_revision != state.requested_revision
                        || hint.event_watermark != state.admitted_watermark,
                ),
                Phase::Idle | Phase::Armed(_) => 1,
            };
            if let Some(requested_revision) = pending_requested_revision.take() {
                observations.request_revision_checks += 1;
                observations.lost_request_revisions += match &state.phase {
                    Phase::Outstanding(outstanding) => {
                        usize::from(outstanding.covered_revision < requested_revision)
                    }
                    Phase::Idle | Phase::Armed(_) => 1,
                };
            }
            if opportunity + 1 < 10_000 && opportunity.is_multiple_of(7) {
                assert!(scheduler.request_render(&actor));
                pending_requested_revision =
                    Some(scheduler.actors.get(&actor).unwrap().requested_revision);
            }
            let render_cost = if opportunity.is_multiple_of(11) {
                hint.submission_deadline_ns
                    .saturating_sub(dispatch)
                    .saturating_add(1)
            } else if opportunity.is_multiple_of(5) {
                3_000_000
            } else {
                1_000_000
            };
            scheduler
                .finish(hint.token, dispatch.saturating_add(render_cost), &[])
                .unwrap();
            for missed in scheduler.drain_deadline_results() {
                predictor.note_deadline_result(missed);
            }
            presented = presented.saturating_add(HZ_60);
        }
        let state = scheduler.actors.get(&actor).unwrap();
        observations.lost_request_revisions += usize::from(pending_requested_revision.is_some());
        observations.lost_request_revisions +=
            usize::from(state.requested_revision != state.satisfied_revision);
        assert_eq!(scheduler.metrics().protocol_errors, 0);
        assert_eq!(scheduler.metrics().catch_up_bursts, 0);
        assert!(scheduler.metrics().late > 0);
        emit_deterministic("cost_and_retarget", 10_000, observations);
    }

    #[test]
    fn bounded_dispatch_rotation_gives_background_actors_progress() {
        let mut scheduler = RenderScheduler::default();
        let predictor = FramePredictor::new(HZ_60);
        for index in 0..100 {
            let actor = ActorScope::new(format!("background-{index:03}"), 1);
            assert!(scheduler.register(actor.clone()));
            assert!(scheduler.set_relevant_panes(&actor, [index + 1]));
            assert!(scheduler.set_subscription(&actor, true));
        }
        scheduler.arm_all(0, &predictor, 1_000_000);
        let deadline = scheduler.next_deadline_ns().unwrap();
        let first = scheduler.dispatch_due(deadline);
        let second = scheduler.dispatch_due(deadline);
        assert_eq!(first.len(), MAX_DISPATCH_PER_TURN);
        assert_eq!(second.len(), 100 - MAX_DISPATCH_PER_TURN);
        let actors = first
            .into_iter()
            .chain(second)
            .map(|(actor, _hint)| actor)
            .collect::<BTreeSet<_>>();
        assert_eq!(actors.len(), 100);
    }

    #[test]
    fn one_infeasible_target_advances_one_interval_without_a_burst() {
        let mut predictor = FramePredictor::new(HZ_60);
        predictor.observe_presented(0);
        let feasible = predictor.plan(0, 1_000_000, 1_000_000);
        let missed_now = feasible.dispatch_ns.saturating_add(1);
        let advanced = predictor.plan(missed_now, 1_000_000, 1_000_000);
        assert_eq!(advanced.target_ns, feasible.target_ns + HZ_60);
    }

    #[test]
    fn deadline_feedback_and_planned_dispatch_lateness_converge_under_jitter() {
        let (mut scheduler, actor, mut predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));
        let mut presented = 0_u64;
        let mut misses = Vec::new();
        let mut final_lead = 0_u64;

        for opportunity in 0..512_usize {
            for missed in scheduler.drain_deadline_results() {
                predictor.note_deadline_result(missed);
            }
            predictor.observe_presented(presented);
            assert!(scheduler.arm(&actor, presented, &predictor, 1_000_000));
            let planned_dispatch = scheduler.next_deadline_ns().unwrap();
            let (wake_lateness, render_cost) = if opportunity == 0 {
                (1_000_000, 2_000_000)
            } else {
                *[
                    (100_000, 2_100_000),
                    (300_000, 2_100_000),
                    (200_000, 2_100_000),
                    (400_000, 2_100_000),
                ]
                .get(opportunity % 4)
                .unwrap()
            };
            let actual_dispatch = planned_dispatch.saturating_add(wake_lateness);
            let hint = scheduler.dispatch_due(actual_dispatch).pop().unwrap().1;
            let accepted_at = actual_dispatch.saturating_add(render_cost);
            scheduler.finish(hint.token, accepted_at, &[]).unwrap();
            let missed = accepted_at > hint.submission_deadline_ns;
            misses.push(missed);
            final_lead = hint
                .target_presentation_time_ns
                .saturating_sub(planned_dispatch);
            presented = presented.saturating_add(HZ_60);
        }

        for missed in scheduler.drain_deadline_results() {
            predictor.note_deadline_result(missed);
        }
        assert_eq!(misses.first(), Some(&true));
        assert_eq!(misses.iter().filter(|missed| **missed).count(), 1);
        assert!(misses.iter().rev().take(100).all(|missed| !missed));
        assert!(final_lead > COLD_GUEST_COST_NS + COLD_HOST_COST_NS + 500_000);
        assert_eq!(scheduler.metrics().late, 1);
        assert_eq!(predictor.safety_margin_ns, 500_000);
    }

    #[test]
    fn estimated_present_uses_frame_cell_end_but_one_frame_late_misses() {
        let (mut scheduler, actor, predictor) = ready_scheduler();
        assert!(scheduler.set_subscription(&actor, true));

        let jittered = dispatch_one(&mut scheduler, &actor, &predictor, 0);
        scheduler
            .finish(jittered.token, jittered.submission_deadline_ns, &[11])
            .unwrap();
        let jittered_end = scheduler
            .active_traces
            .get(&jittered.token)
            .unwrap()
            .opportunity_end_ns;
        assert!(jittered_end > jittered.target_presentation_time_ns);
        release_and_compose(
            &mut scheduler,
            jittered.token,
            jittered_end.saturating_sub(2),
        );
        submit_and_present(&mut scheduler, jittered.token, jittered_end, true);
        let jittered_trace = scheduler.completed_traces().back().unwrap();
        assert_eq!(jittered_trace.target_presented, Some(true));
        assert_eq!(
            jittered_trace.settlement,
            Some(Settlement::Presented { estimated: true })
        );

        let one_frame_late = dispatch_one(
            &mut scheduler,
            &actor,
            &predictor,
            jittered.target_presentation_time_ns,
        );
        scheduler
            .finish(
                one_frame_late.token,
                one_frame_late.submission_deadline_ns,
                &[11],
            )
            .unwrap();
        let late_presentation = one_frame_late
            .target_presentation_time_ns
            .saturating_add(HZ_60);
        release_and_compose(
            &mut scheduler,
            one_frame_late.token,
            late_presentation.saturating_sub(2),
        );
        submit_and_present(
            &mut scheduler,
            one_frame_late.token,
            late_presentation,
            true,
        );
        let late_trace = scheduler.completed_traces().back().unwrap();
        assert_eq!(late_trace.target_presented, Some(false));
        assert_eq!(
            late_trace.settlement,
            Some(Settlement::Presented { estimated: true })
        );
        assert_eq!(scheduler.metrics().intended_target_misses, 1);
    }

    #[test]
    fn atomic_frames_promote_visible_panes_together_and_ignore_hidden_barrier() {
        let actor = ActorScope::new("multi", 1);
        let visible = BTreeSet::from([1, 2]);
        let mut frames = AtomicActorFrames::default();
        assert_eq!(
            frames.stage(
                actor.clone(),
                9,
                RenderToken(4),
                vec![(1, "a"), (2, "b"), (3, "c")],
                &visible
            ),
            Ok(None)
        );
        assert!(frames.mark_ready(&actor, 9, 1));
        assert_eq!(frames.promote_ready(&actor, &visible), None);
        assert_eq!(frames.visible(1), None);
        assert!(frames.mark_ready(&actor, 9, 2));
        assert_eq!(frames.promote_ready(&actor, &visible), Some(RenderToken(4)));
        assert_eq!(frames.visible(1), Some(&"a"));
        assert_eq!(frames.visible(2), Some(&"b"));
        assert!(frames.expose(3));
        assert_eq!(frames.visible(3), Some(&"c"));
    }

    #[test]
    fn atomic_batch_rejects_duplicate_panes_without_mutation() {
        let actor = ActorScope::new("multi", 1);
        let mut frames = AtomicActorFrames::default();
        assert_eq!(
            frames.stage(
                actor,
                1,
                RenderToken(1),
                vec![(1, "a"), (1, "b")],
                &BTreeSet::from([1])
            ),
            Err(FrameStageError::LimitOrDuplicate)
        );
        assert_eq!(frames.visible(1), None);
    }
}
