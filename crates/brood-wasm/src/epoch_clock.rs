//! Shared wall-clock driver for Wasmtime epoch interruption.
//!
//! Epoch interruption needs a clock while guest code is runnable, but a
//! dedicated 2 ms timer thread per actor makes a completely idle session wake
//! once per actor on every tick. One process-wide clock preserves each
//! engine's configured cadence while coalescing equal deadlines into a single
//! wakeup.

use std::{
    collections::HashMap,
    future::{Future, poll_fn},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, ensure};
use wasmtime::Engine;

static SHARED_EPOCH_CLOCK: OnceLock<Result<SharedEpochClock, String>> = OnceLock::new();

pub struct EpochTicker {
    registration: Arc<EpochRegistration>,
}

impl EpochTicker {
    pub fn start(engine: Engine, interval: Duration) -> Result<Self> {
        ensure!(!interval.is_zero(), "epoch tick interval must be non-zero");
        let clock = SHARED_EPOCH_CLOCK
            .get_or_init(|| SharedEpochClock::start().map_err(|error| error.to_string()))
            .as_ref()
            .map_err(|error| anyhow!("start shared epoch clock: {error}"))?
            .clone();
        let idle_grace = interval.saturating_mul(4).max(Duration::from_millis(8));
        let registration = clock.register(engine, interval, idle_grace);
        Ok(Self {
            registration: Arc::new(EpochRegistration {
                clock,
                id: registration.id,
                active_poll: registration.active_poll,
            }),
        })
    }

    /// Poll a Wasmtime future with its epoch clock armed while guest code can
    /// actually be executing. A short grace period coalesces adjacent polls;
    /// a pending guest does not keep waking the process while it waits for
    /// input or I/O.
    pub async fn drive<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        poll_fn(|context| {
            let _active = ActiveEpochPoll::enter(&self.registration);
            future.as_mut().poll(context)
        })
        .await
    }
}

impl Clone for EpochTicker {
    fn clone(&self) -> Self {
        Self {
            registration: Arc::clone(&self.registration),
        }
    }
}

struct EpochRegistration {
    clock: SharedEpochClock,
    id: u64,
    active_poll: Arc<AtomicBool>,
}

impl Drop for EpochRegistration {
    fn drop(&mut self) {
        self.clock.unregister(self.id);
    }
}

struct ActiveEpochPoll<'a> {
    registration: &'a EpochRegistration,
}

impl<'a> ActiveEpochPoll<'a> {
    fn enter(registration: &'a EpochRegistration) -> Self {
        registration.active_poll.store(true, Ordering::Release);
        registration.clock.wake();
        Self { registration }
    }
}

impl Drop for ActiveEpochPoll<'_> {
    fn drop(&mut self) {
        self.registration
            .active_poll
            .store(false, Ordering::Release);
    }
}

#[derive(Clone)]
struct SharedEpochClock {
    shared: Arc<SharedState>,
}

struct SharedState {
    state: Mutex<ClockState>,
    changed: Condvar,
    awake: AtomicBool,
}

#[derive(Default)]
struct ClockState {
    next_registration: u64,
    registrations: HashMap<u64, EngineRegistration>,
}

struct EngineRegistration {
    engine: Engine,
    interval: Duration,
    idle_grace: Duration,
    next_tick: Instant,
    active_poll: Arc<AtomicBool>,
    grace_until: Option<Instant>,
    scheduled_active: bool,
    #[cfg(test)]
    ticks: u64,
}

impl SharedEpochClock {
    fn start() -> Result<Self> {
        let shared = Arc::new(SharedState {
            state: Mutex::new(ClockState::default()),
            changed: Condvar::new(),
            awake: AtomicBool::new(false),
        });
        let thread_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("brood-wasm-epoch".to_string())
            .spawn(move || run_clock(&thread_shared))
            .context("spawn shared Wasmtime epoch clock")?;
        Ok(Self { shared })
    }

    fn register(
        &self,
        engine: Engine,
        interval: Duration,
        idle_grace: Duration,
    ) -> RegisteredEngine {
        let mut state = lock_state(&self.shared);
        let registration = state.next_registration;
        state.next_registration = state.next_registration.wrapping_add(1);
        let active_poll = Arc::new(AtomicBool::new(false));
        state.registrations.insert(
            registration,
            EngineRegistration {
                engine,
                interval,
                idle_grace,
                next_tick: Instant::now() + interval,
                active_poll: Arc::clone(&active_poll),
                grace_until: None,
                scheduled_active: false,
                #[cfg(test)]
                ticks: 0,
            },
        );
        drop(state);
        self.shared.changed.notify_one();
        RegisteredEngine {
            id: registration,
            active_poll,
        }
    }

    fn unregister(&self, registration: u64) {
        let mut state = lock_state(&self.shared);
        state.registrations.remove(&registration);
        drop(state);
        self.shared.changed.notify_one();
    }

    fn wake(&self) {
        if self.shared.awake.load(Ordering::Acquire)
            || self.shared.awake.swap(true, Ordering::AcqRel)
        {
            return;
        }
        // Taking the clock mutex pairs this notification with the condition
        // variable wait, so an activation cannot be lost between the clock's
        // empty scan and its sleep.
        drop(lock_state(&self.shared));
        self.shared.changed.notify_one();
    }

    #[cfg(test)]
    fn tick_count(&self, registration: u64) -> u64 {
        lock_state(&self.shared)
            .registrations
            .get(&registration)
            .map_or(0, |registration| registration.ticks)
    }
}

struct RegisteredEngine {
    id: u64,
    active_poll: Arc<AtomicBool>,
}

fn run_clock(shared: &SharedState) {
    let mut state = lock_state(shared);
    loop {
        let now = Instant::now();
        let mut due = Vec::new();
        let mut next_deadline = None;
        for registration in state.registrations.values_mut() {
            if !registration_is_active(registration, now) {
                registration.scheduled_active = false;
                continue;
            }
            if !registration.scheduled_active {
                registration.next_tick = now + registration.interval;
                registration.scheduled_active = true;
            }
            if registration.next_tick <= now {
                due.push(registration.engine.clone());
                #[cfg(test)]
                {
                    registration.ticks = registration.ticks.saturating_add(1);
                }
                // Missed ticks do not need replaying: epoch deadlines only
                // require forward progress, and replay would create a burst
                // after scheduler suspension.
                registration.next_tick = now + registration.interval;
            }
            next_deadline = Some(
                next_deadline.map_or(registration.next_tick, |deadline: Instant| {
                    deadline.min(registration.next_tick)
                }),
            );
        }

        if !due.is_empty() {
            drop(state);
            for engine in due {
                engine.increment_epoch();
            }
            state = lock_state(shared);
            continue;
        }

        state = if let Some(deadline) = next_deadline {
            let timeout = deadline.saturating_duration_since(Instant::now());
            match shared.changed.wait_timeout(state, timeout) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            }
        } else {
            shared.awake.store(false, Ordering::Release);
            let now = Instant::now();
            if state
                .registrations
                .values_mut()
                .any(|registration| registration_is_active(registration, now))
            {
                // An activation raced with disarming. Keep driving it
                // without entering the condition-variable wait.
                shared.awake.store(true, Ordering::Release);
                state
            } else {
                let state = match shared.changed.wait(state) {
                    Ok(state) => state,
                    Err(poisoned) => poisoned.into_inner(),
                };
                shared.awake.store(true, Ordering::Release);
                state
            }
        };
    }
}

fn registration_is_active(registration: &mut EngineRegistration, now: Instant) -> bool {
    if registration.active_poll.load(Ordering::Acquire) {
        registration.grace_until = Some(now + registration.idle_grace);
        true
    } else {
        registration
            .grace_until
            .is_some_and(|grace_until| grace_until > now)
    }
}

fn lock_state(shared: &SharedState) -> std::sync::MutexGuard<'_, ClockState> {
    match shared.state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::Config;

    #[test]
    fn clock_ticks_only_while_guest_poll_is_active() -> Result<()> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let ticker = EpochTicker::start(Engine::new(&config)?, Duration::from_millis(2))?;
        let clock = &ticker.registration.clock;
        let registration = ticker.registration.id;

        thread::sleep(Duration::from_millis(12));
        ensure!(clock.tick_count(registration) == 0);

        let active = ActiveEpochPoll::enter(&ticker.registration);
        thread::sleep(Duration::from_millis(12));
        ensure!(clock.tick_count(registration) > 0);
        drop(active);

        // Let the short post-poll grace period expire before measuring the
        // dormant interval.
        thread::sleep(Duration::from_millis(12));
        let dormant_ticks = clock.tick_count(registration);
        thread::sleep(Duration::from_millis(12));
        ensure!(clock.tick_count(registration) == dormant_ticks);
        Ok(())
    }
}
