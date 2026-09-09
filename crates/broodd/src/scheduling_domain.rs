//! Bounded accounting primitives for the session-server scheduling domains.
//!
//! The Smithay owner records into these structures synchronously. Snapshot
//! calculation is intentionally kept off the hot path.

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    fmt,
    time::Duration,
};

/// Maximum retained Smithay callback samples after an explicit measurement
/// reset. Acceptance rejects an overflow rather than reporting a moving tail.
pub const SMITHAY_CALLBACK_SAMPLE_CAPACITY: usize = 65_536;

/// Frozen callback budget used by the render-scheduling acceptance lane.
pub const SMITHAY_CALLBACK_BUDGET: Duration = Duration::from_micros(2_500);

/// Maximum backend actor events reduced during one calloop service turn.
pub const BACKEND_ACTOR_EVENTS_PER_TURN: usize = 64;

/// Per-actor quantum prevents one continuously emitting actor from occupying
/// the complete Smithay service budget.
pub const BACKEND_ACTOR_EVENTS_PER_ACTOR: usize = 8;

/// Maximum current-generation actor progress entries exposed to diagnostics.
pub const BACKEND_ACTOR_PROGRESS_CAPACITY: usize = 256;

/// Bounded current-generation progress counters for actor-turn diagnostics.
#[derive(Debug, Default)]
pub struct ActorProgressMetrics {
    entries: BTreeMap<String, (u64, u64)>,
}

impl ActorProgressMetrics {
    pub fn record(&mut self, actor: &str, generation: u64) {
        if let Some((recorded_generation, completed)) = self.entries.get_mut(actor) {
            if *recorded_generation != generation {
                *recorded_generation = generation;
                *completed = 0;
            }
            *completed = completed.saturating_add(1);
        } else if self.entries.len() < BACKEND_ACTOR_PROGRESS_CAPACITY {
            self.entries.insert(actor.to_string(), (generation, 1));
        }
    }

    pub fn retain_current(&mut self, mut is_current: impl FnMut(&str, u64) -> bool) {
        self.entries
            .retain(|actor, (generation, _)| is_current(actor, *generation));
    }

    #[must_use]
    pub fn snapshot(&self, mut is_current: impl FnMut(&str, u64) -> bool) -> BTreeMap<String, u64> {
        self.entries
            .iter()
            .filter(|(actor, (generation, _))| is_current(actor, *generation))
            .map(|(actor, (_, completed))| (actor.clone(), *completed))
            .collect()
    }
}

/// Maximum presentation-to-backend actor messages admitted during one source callback.
pub const BACKEND_ACTOR_MESSAGES_PER_CALLBACK: usize = 64;

/// Maximum presentation backend requests admitted during one source callback.
pub const BACKEND_REQUESTS_PER_CALLBACK: usize = 8;

/// Input has the largest callback quantum but remains finite under a producer
/// flood so frame and Wayland callbacks regain the owner thread.
pub const SMITHAY_INPUT_EVENTS_PER_CALLBACK: usize = 128;

pub const BACKEND_LIFECYCLE_CAPACITY: usize = 64;
pub const BACKEND_LIFECYCLE_START_LIMIT: usize = 48;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LifecycleAdmission {
    active: usize,
}

impl LifecycleAdmission {
    pub const fn try_admit_start(&mut self) -> bool {
        if self.active >= BACKEND_LIFECYCLE_START_LIMIT {
            return false;
        }
        self.active += 1;
        true
    }

    pub const fn try_admit_reserved(&mut self) -> bool {
        if self.active >= BACKEND_LIFECYCLE_CAPACITY {
            return false;
        }
        self.active += 1;
        true
    }

    pub const fn complete(&mut self) {
        self.active = self.active.saturating_sub(1);
    }

    #[must_use]
    pub const fn active(self) -> usize {
        self.active
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleRequestIdError {
    Invalid,
    Exhausted,
}

impl fmt::Display for LifecycleRequestIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => formatter.write_str("request ID is zero"),
            Self::Exhausted => formatter.write_str("request ID space is exhausted"),
        }
    }
}

impl Error for LifecycleRequestIdError {}

/// Atomically admit one lifecycle start and advance its request ID.
///
/// ID validation happens before admission. A full lane or exhausted ID space
/// leaves both inputs unchanged, so callers cannot leak capacity or consume an
/// identifier on a rejected start.
pub fn try_admit_lifecycle_request_id(
    next: &mut u64,
    admission: &mut LifecycleAdmission,
) -> Result<Option<u64>, LifecycleRequestIdError> {
    let request = *next;
    if request == 0 {
        return Err(LifecycleRequestIdError::Invalid);
    }
    let following = request
        .checked_add(1)
        .ok_or(LifecycleRequestIdError::Exhausted)?;
    if !admission.try_admit_start() {
        return Ok(None);
    }
    *next = following;
    Ok(Some(request))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendWorkerPartition {
    pub actors: usize,
    pub general_io: usize,
}

/// Split ordinary Tokio and actor workers into independent domains.
///
/// Small machines use one worker per runtime; larger machines reserve roughly
/// one third, capped at four, for actors.
#[must_use]
pub fn backend_worker_partition(available: usize) -> BackendWorkerPartition {
    let available = available.max(1);
    let actors = if available < 4 {
        1
    } else {
        (available / 3).clamp(1, 4)
    };
    let general_io = available.saturating_sub(actors).clamp(1, 8);
    BackendWorkerPartition { actors, general_io }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BoundedDrainPlan {
    pub actors: Vec<(usize, usize)>,
    pub next_cursor: usize,
    pub unvisited_actors: usize,
}

/// Build one rotating, per-owner-quantized drain plan. The returned admission
/// sum never exceeds `total_budget` and every non-empty plan advances beyond
/// the actors it visited.
#[must_use]
pub fn bounded_round_robin_plan(
    actor_count: usize,
    cursor: usize,
    total_budget: usize,
    per_actor_budget: usize,
) -> BoundedDrainPlan {
    if actor_count == 0 || total_budget == 0 || per_actor_budget == 0 {
        return BoundedDrainPlan::default();
    }
    let start = cursor % actor_count;
    let mut remaining = total_budget;
    let mut actors = Vec::with_capacity(actor_count.min(total_budget.div_ceil(per_actor_budget)));
    for offset in 0..actor_count {
        if remaining == 0 {
            break;
        }
        let budget = remaining.min(per_actor_budget);
        actors.push(((start + offset) % actor_count, budget));
        remaining -= budget;
    }
    let visited = actors.len();
    BoundedDrainPlan {
        actors,
        next_cursor: (start + visited) % actor_count,
        unvisited_actors: actor_count.saturating_sub(visited),
    }
}

/// Count observer events which remain ready after a bounded actor drain.
///
/// Actor population is deliberately absent from this calculation: actors which
/// were outside one round-robin quantum do not require another callback unless
/// their observer queue actually contains work.
#[must_use]
pub fn observed_actor_event_backlog(pending_events: impl IntoIterator<Item = usize>) -> usize {
    pending_events
        .into_iter()
        .fold(0usize, usize::saturating_add)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CallbackMetricsSnapshot {
    pub callbacks: u64,
    pub over_budget: u64,
    pub lifetime_over_budget: u64,
    pub last_micros: u64,
    pub maximum_micros: u64,
    pub lifetime_maximum_micros: u64,
    pub p99_micros: u64,
    pub retained_samples: usize,
    pub sample_capacity: usize,
}

/// Fixed-retention callback timing. Recording performs no allocation after the
/// first `SMITHAY_CALLBACK_SAMPLE_CAPACITY` callbacks.
#[derive(Debug)]
pub struct CallbackMetrics {
    samples_micros: VecDeque<u64>,
    callbacks: u64,
    lifetime_over_budget: u64,
    last_micros: u64,
    lifetime_maximum_micros: u64,
}

impl Default for CallbackMetrics {
    fn default() -> Self {
        Self {
            samples_micros: VecDeque::with_capacity(SMITHAY_CALLBACK_SAMPLE_CAPACITY),
            callbacks: 0,
            lifetime_over_budget: 0,
            last_micros: 0,
            lifetime_maximum_micros: 0,
        }
    }
}

impl CallbackMetrics {
    pub fn record(&mut self, elapsed: Duration) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.callbacks = self.callbacks.saturating_add(1);
        self.last_micros = micros;
        self.lifetime_maximum_micros = self.lifetime_maximum_micros.max(micros);
        if elapsed > SMITHAY_CALLBACK_BUDGET {
            self.lifetime_over_budget = self.lifetime_over_budget.saturating_add(1);
        }
        if self.samples_micros.len() == SMITHAY_CALLBACK_SAMPLE_CAPACITY {
            self.samples_micros.pop_front();
        }
        self.samples_micros.push_back(micros);
    }

    #[must_use]
    pub fn snapshot(&self) -> CallbackMetricsSnapshot {
        let mut samples = self.samples_micros.iter().copied().collect::<Vec<_>>();
        samples.sort_unstable();
        let p99_micros = percentile(&samples, 99);
        let maximum_micros = samples.last().copied().unwrap_or(0);
        let budget_micros = u64::try_from(SMITHAY_CALLBACK_BUDGET.as_micros()).unwrap_or(u64::MAX);
        let over_budget = u64::try_from(
            samples
                .iter()
                .filter(|sample| **sample > budget_micros)
                .count(),
        )
        .unwrap_or(u64::MAX);
        CallbackMetricsSnapshot {
            callbacks: self.callbacks,
            over_budget,
            lifetime_over_budget: self.lifetime_over_budget,
            last_micros: self.last_micros,
            maximum_micros,
            lifetime_maximum_micros: self.lifetime_maximum_micros,
            p99_micros,
            retained_samples: samples.len(),
            sample_capacity: SMITHAY_CALLBACK_SAMPLE_CAPACITY,
        }
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = sorted
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1);
    sorted.get(rank).copied().unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BoundedWorkMetrics {
    pub admitted: u64,
    pub completed: u64,
    pub deferred_turns: u64,
    pub dropped: u64,
    pub last_batch: usize,
    pub maximum_batch: usize,
    /// Largest ready batch observed at a drain boundary. This is a lower bound
    /// when the producer keeps additional items in its own bounded channel.
    pub maximum_backlog: usize,
    pub current_depth: usize,
}

impl BoundedWorkMetrics {
    pub fn record_batch(&mut self, completed: usize, observed_backlog: usize) {
        let completed_u64 = u64::try_from(completed).unwrap_or(u64::MAX);
        self.admitted = self.admitted.saturating_add(completed_u64);
        self.completed = self.completed.saturating_add(completed_u64);
        self.last_batch = completed;
        self.maximum_batch = self.maximum_batch.max(completed);
        self.maximum_backlog = self.maximum_backlog.max(observed_backlog);
        if observed_backlog > 0 {
            self.deferred_turns = self.deferred_turns.saturating_add(1);
        }
    }

    pub fn record_admission(&mut self, depth: usize) {
        self.admitted = self.admitted.saturating_add(1);
        self.current_depth = depth;
        self.maximum_backlog = self.maximum_backlog.max(depth);
    }

    pub const fn record_completion(&mut self, depth: usize) {
        self.completed = self.completed.saturating_add(1);
        self.current_depth = depth;
    }
}

#[cfg(test)]
mod tests {
    use super::{bounded_round_robin_plan, observed_actor_event_backlog};

    #[test]
    fn unvisited_idle_actors_do_not_create_an_observer_backlog() {
        let actor_count = 9;
        let plan = bounded_round_robin_plan(actor_count, 0, 64, 8);
        assert_eq!(plan.actors.len(), 8);
        assert_eq!(plan.unvisited_actors, 1);

        let pending = vec![0; actor_count];
        assert_eq!(observed_actor_event_backlog(pending), 0);
    }

    #[test]
    fn observer_backlog_counts_only_events_still_queued() {
        assert_eq!(observed_actor_event_backlog([0, 2, 0, 3]), 5);
    }
}
