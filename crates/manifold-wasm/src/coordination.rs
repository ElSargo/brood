//! Generation-scoped notifications and worker pollables.
//!
//! The guest owns synchronization data in shared linear memory. This module
//! only carries monotonic publication generations into the host scheduler so
//! actors can receive coalesced events and workers can suspend without
//! spinning.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use tokio::sync::{mpsc, watch};

static NEXT_NOTIFICATION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_POLLABLE_ID: AtomicU64 = AtomicU64::new(1);

/// Host notification identity. IDs are globally unique for the daemon
/// lifetime, while ownership and teardown remain actor-generation scoped.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NotificationId(u64);

impl NotificationId {
    /// Reconstruct a guest-visible handle.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the guest-visible handle.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Host pollable identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PollableId(u64);

impl PollableId {
    /// Reconstruct a guest-visible handle.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the guest-visible handle.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Bounds for one actor generation's coordination table.
#[derive(Clone, Copy, Debug)]
pub struct CoordinationConfig {
    /// Maximum live notifications.
    pub max_notifications: usize,
    /// Maximum live pollables.
    pub max_pollables: usize,
    /// Maximum pollables in one worker wait.
    pub max_wait_set: usize,
}

impl Default for CoordinationConfig {
    fn default() -> Self {
        Self {
            max_notifications: 64,
            max_pollables: 256,
            max_wait_set: 64,
        }
    }
}

/// Notification table failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum NotificationError {
    /// The owning actor generation is stopping.
    #[error("actor generation has stopped")]
    Stopped,
    /// A configured resource bound was reached.
    #[error("notification resource limit reached")]
    LimitReached,
    /// A reserved actor-event lane could not admit another unique event.
    #[error("notification event backlog is full")]
    BacklogFull,
    /// The handle does not belong to this live actor generation.
    #[error("notification is stale or foreign")]
    InvalidHandle,
    /// Generation zero or another invalid generation was announced.
    #[error("invalid notification generation")]
    InvalidGeneration,
    /// This one-shot actor watch has not yet delivered.
    #[error("notification already has an actor watch")]
    AlreadyWatched,
}

/// Worker polling failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PollError {
    /// The owning actor generation is stopping.
    #[error("actor generation has stopped")]
    Stopped,
    /// A configured resource or wait-set bound was reached.
    #[error("poll resource limit reached")]
    LimitReached,
    /// A pollable does not belong to this live actor generation.
    #[error("pollable is stale or foreign")]
    InvalidPollable,
    /// One pollable appeared more than once in a wait set.
    #[error("wait set contains a duplicate pollable")]
    DuplicatePollable,
    /// The wait set was empty.
    #[error("wait set is empty")]
    EmptyWaitSet,
    /// The relative deadline elapsed before readiness.
    #[error("worker wait timed out")]
    TimedOut,
    /// The worker was cancelled.
    #[error("worker wait was cancelled")]
    Cancelled,
}

/// Result of registering one actor watch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActorWatchResult {
    /// Interest was armed and no newer generation was known.
    Armed,
    /// Readiness was already known and an actor event was queued.
    Queued { generation: u64 },
}

/// Coalesced one-shot actor notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActorNotification {
    /// Notification which may have changed.
    pub notification: NotificationId,
    /// Greatest generation announced before the event left the host queue.
    pub generation: u64,
    /// Opaque token supplied while arming the actor watch.
    pub token: u64,
}

/// Worker wait ordering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitMode {
    /// Prefer the lowest input index when multiple items are ready.
    Biased,
    /// Rotate the preferred input index between waits.
    Fair,
}

/// One ready pollable returned to a worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadyPollable {
    /// Index in the submitted wait set.
    pub index: usize,
    /// Ready pollable handle.
    pub pollable: PollableId,
    /// Latest announced notification generation.
    pub generation: u64,
}

/// Runtime-observable coordination counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoordinationStats {
    /// Live notification resources.
    pub notifications: usize,
    /// Live pollable resources.
    pub pollables: usize,
    /// Pollable registrations currently suspended in worker waits.
    pub wait_registrations: usize,
    /// Actor notifications currently coalesced in the reserved lane.
    pub pending_actor_events: usize,
}

/// Actor-generation-owned coordination table.
#[derive(Clone)]
pub struct CoordinationHub {
    inner: Arc<CoordinationInner>,
}

struct CoordinationInner {
    generation: u64,
    config: CoordinationConfig,
    alive: AtomicBool,
    state: Mutex<CoordinationState>,
    actor_events: mpsc::Sender<NotificationId>,
    shutdown: watch::Sender<bool>,
    wait_registrations: AtomicUsize,
    pending_actor_events: AtomicUsize,
    fair_cursor: AtomicU64,
}

#[derive(Default)]
struct CoordinationState {
    notifications: HashMap<NotificationId, NotificationEntry>,
    pollables: HashMap<PollableId, NotificationPollable>,
}

struct NotificationEntry {
    latest: u64,
    changes: watch::Sender<u64>,
    actor_watch: Option<ActorWatch>,
    actor_event_queued: bool,
}

#[derive(Clone, Copy)]
struct ActorWatch {
    token: u64,
}

struct NotificationPollable {
    notification: NotificationId,
    observed_generation: u64,
    invalidated: watch::Sender<bool>,
}

struct PreparedPollable {
    index: usize,
    pollable: PollableId,
    observed_generation: u64,
    changes: watch::Receiver<u64>,
    invalidated: watch::Receiver<bool>,
}

struct WaitRegistrationGuard {
    inner: Arc<CoordinationInner>,
    count: usize,
}

impl Drop for WaitRegistrationGuard {
    fn drop(&mut self) {
        self.inner
            .wait_registrations
            .fetch_sub(self.count, Ordering::AcqRel);
    }
}

impl CoordinationHub {
    /// Create one generation-scoped hub and its reserved actor event receiver.
    pub fn new(
        generation: u64,
        config: CoordinationConfig,
    ) -> Result<(Self, mpsc::Receiver<NotificationId>), NotificationError> {
        if generation == 0
            || config.max_notifications == 0
            || config.max_pollables == 0
            || config.max_wait_set == 0
        {
            return Err(NotificationError::InvalidGeneration);
        }
        let (actor_events, receiver) = mpsc::channel(config.max_notifications);
        let (shutdown, _shutdown_receiver) = watch::channel(false);
        Ok((
            Self {
                inner: Arc::new(CoordinationInner {
                    generation,
                    config,
                    alive: AtomicBool::new(true),
                    state: Mutex::new(CoordinationState::default()),
                    actor_events,
                    shutdown,
                    wait_registrations: AtomicUsize::new(0),
                    pending_actor_events: AtomicUsize::new(0),
                    fair_cursor: AtomicU64::new(0),
                }),
            },
            receiver,
        ))
    }

    /// Owning actor generation.
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// Allocate a notification with initial generation zero.
    pub fn create_notification(&self) -> Result<NotificationId, NotificationError> {
        self.ensure_alive()?;
        let mut state = self.lock_state()?;
        if state.notifications.len() >= self.inner.config.max_notifications {
            return Err(NotificationError::LimitReached);
        }
        let notification = NotificationId(allocate_global_id(&NEXT_NOTIFICATION_ID)?);
        let (changes, _receiver) = watch::channel(0);
        state.notifications.insert(
            notification,
            NotificationEntry {
                latest: 0,
                changes,
                actor_watch: None,
                actor_event_queued: false,
            },
        );
        drop(state);
        Ok(notification)
    }

    /// Drop a notification and every pollable derived from it.
    pub fn drop_notification(&self, notification: NotificationId) -> Result<(), NotificationError> {
        self.ensure_alive()?;
        let mut state = self.lock_state()?;
        let entry = state
            .notifications
            .get(&notification)
            .ok_or(NotificationError::InvalidHandle)?;
        if entry.actor_event_queued {
            return Err(NotificationError::AlreadyWatched);
        }
        let _removed = state.notifications.remove(&notification);
        state.pollables.retain(|_id, pollable| {
            if pollable.notification == notification {
                pollable.invalidated.send_replace(true);
                false
            } else {
                true
            }
        });
        drop(state);
        Ok(())
    }

    /// Announce that guest shared state was published through `generation`.
    /// Out-of-order announcements are accepted and return the greatest value.
    pub fn signal(
        &self,
        notification: NotificationId,
        generation: u64,
    ) -> Result<u64, NotificationError> {
        self.ensure_alive()?;
        if generation == 0 {
            return Err(NotificationError::InvalidGeneration);
        }
        let mut state = self.lock_state()?;
        let entry = state
            .notifications
            .get_mut(&notification)
            .ok_or(NotificationError::InvalidHandle)?;
        let advanced = generation > entry.latest;
        if advanced {
            entry.latest = generation;
            entry.changes.send_replace(generation);
        }
        if advanced && entry.actor_watch.is_some() && !entry.actor_event_queued {
            self.inner
                .actor_events
                .try_send(notification)
                .map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => NotificationError::BacklogFull,
                    mpsc::error::TrySendError::Closed(_) => NotificationError::Stopped,
                })?;
            entry.actor_event_queued = true;
            self.inner
                .pending_actor_events
                .fetch_add(1, Ordering::AcqRel);
        }
        let latest = entry.latest;
        drop(state);
        Ok(latest)
    }

    /// Atomically check the latest generation and arm a one-shot actor watch.
    pub fn watch_actor(
        &self,
        notification: NotificationId,
        observed_generation: u64,
        token: u64,
    ) -> Result<ActorWatchResult, NotificationError> {
        self.ensure_alive()?;
        let mut state = self.lock_state()?;
        let entry = state
            .notifications
            .get_mut(&notification)
            .ok_or(NotificationError::InvalidHandle)?;
        if entry.actor_watch.is_some() {
            return Err(NotificationError::AlreadyWatched);
        }
        entry.actor_watch = Some(ActorWatch { token });
        let result = if entry.latest == observed_generation {
            ActorWatchResult::Armed
        } else {
            if let Err(error) = self.inner.actor_events.try_send(notification) {
                entry.actor_watch = None;
                return Err(match error {
                    mpsc::error::TrySendError::Full(_) => NotificationError::BacklogFull,
                    mpsc::error::TrySendError::Closed(_) => NotificationError::Stopped,
                });
            }
            entry.actor_event_queued = true;
            self.inner
                .pending_actor_events
                .fetch_add(1, Ordering::AcqRel);
            ActorWatchResult::Queued {
                generation: entry.latest,
            }
        };
        drop(state);
        Ok(result)
    }

    /// Remove an actor watch which has not been delivered yet.
    pub fn unwatch_actor(&self, notification: NotificationId) -> Result<bool, NotificationError> {
        self.ensure_alive()?;
        let mut state = self.lock_state()?;
        let entry = state
            .notifications
            .get_mut(&notification)
            .ok_or(NotificationError::InvalidHandle)?;
        if entry.actor_event_queued {
            // The ID is already in the reserved lane. Leave the one-shot live;
            // removing it here would turn an admitted event into a silent drop.
            return Ok(false);
        }
        let removed = entry.actor_watch.take().is_some();
        drop(state);
        Ok(removed)
    }

    /// Convert one queued ID into the latest coalesced actor event.
    pub fn take_actor_event(
        &self,
        notification: NotificationId,
    ) -> Result<Option<ActorNotification>, NotificationError> {
        if !self.inner.alive.load(Ordering::Acquire) {
            return Ok(None);
        }
        let mut state = self.lock_state()?;
        let Some(entry) = state.notifications.get_mut(&notification) else {
            // Dropping a notification is allowed to invalidate an event which
            // was still queued. Its pending count was decremented at drop.
            return Ok(None);
        };
        if !entry.actor_event_queued {
            return Ok(None);
        }
        let Some(watch) = entry.actor_watch.take() else {
            return Ok(None);
        };
        entry.actor_event_queued = false;
        self.inner
            .pending_actor_events
            .fetch_sub(1, Ordering::AcqRel);
        let event = ActorNotification {
            notification,
            generation: entry.latest,
            token: watch.token,
        };
        drop(state);
        Ok(Some(event))
    }

    /// Create a level-triggered pollable for one observed generation.
    pub fn notification_pollable(
        &self,
        notification: NotificationId,
        observed_generation: u64,
    ) -> Result<PollableId, PollError> {
        self.ensure_poll_alive()?;
        let mut state = self.lock_poll_state()?;
        if !state.notifications.contains_key(&notification) {
            return Err(PollError::InvalidPollable);
        }
        if state.pollables.len() >= self.inner.config.max_pollables {
            return Err(PollError::LimitReached);
        }
        let pollable = PollableId(
            allocate_global_id(&NEXT_POLLABLE_ID).map_err(map_notification_to_poll_error)?,
        );
        let (invalidated, _receiver) = watch::channel(false);
        state.pollables.insert(
            pollable,
            NotificationPollable {
                notification,
                observed_generation,
                invalidated,
            },
        );
        drop(state);
        Ok(pollable)
    }

    /// Drop a pollable.
    pub fn drop_pollable(&self, pollable: PollableId) -> Result<(), PollError> {
        self.ensure_poll_alive()?;
        let removed = self
            .lock_poll_state()?
            .pollables
            .remove(&pollable)
            .ok_or(PollError::InvalidPollable)?;
        removed.invalidated.send_replace(true);
        Ok(())
    }

    /// Poll a bounded set without suspending.
    pub fn try_poll(&self, pollables: &[PollableId]) -> Result<Vec<ReadyPollable>, PollError> {
        let prepared = self.prepare_pollables(pollables)?;
        Ok(prepared
            .iter()
            .filter_map(|item| {
                let generation = *item.changes.borrow();
                (generation != item.observed_generation).then_some(ReadyPollable {
                    index: item.index,
                    pollable: item.pollable,
                    generation,
                })
            })
            .collect())
    }

    /// Suspend until one notification changes or the relative timeout elapses.
    /// Dropping this future tears down every losing registration.
    pub async fn wait_any(
        &self,
        pollables: &[PollableId],
        timeout: Option<Duration>,
        mode: WaitMode,
    ) -> Result<ReadyPollable, PollError> {
        let mut prepared = self.prepare_pollables(pollables)?;
        let _registrations = WaitRegistrationGuard {
            inner: Arc::clone(&self.inner),
            count: prepared.len(),
        };
        self.inner
            .wait_registrations
            .fetch_add(prepared.len(), Ordering::AcqRel);

        if prepared.iter().any(|item| *item.invalidated.borrow()) {
            return Err(PollError::InvalidPollable);
        }
        let ready = prepared
            .iter()
            .filter_map(|item| {
                let generation = *item.changes.borrow();
                (generation != item.observed_generation).then_some(ReadyPollable {
                    index: item.index,
                    pollable: item.pollable,
                    generation,
                })
            })
            .collect::<Vec<_>>();
        if !ready.is_empty() {
            return self
                .choose_ready(&ready, prepared.len(), mode)
                .ok_or(PollError::Stopped);
        }

        if mode == WaitMode::Fair {
            let start = usize::try_from(
                self.inner.fair_cursor.fetch_add(1, Ordering::Relaxed)
                    % u64::try_from(prepared.len()).map_err(|_overflow| PollError::LimitReached)?,
            )
            .map_err(|_overflow| PollError::LimitReached)?;
            prepared.rotate_left(start);
        }

        let mut waits = FuturesUnordered::<
            Pin<Box<dyn Future<Output = Result<ReadyPollable, PollError>> + Send>>,
        >::new();
        for mut item in prepared {
            waits.push(
                async move {
                    loop {
                        tokio::select! {
                            biased;
                            _ = item.invalidated.changed() => {
                                return Err(PollError::InvalidPollable);
                            }
                            changed = item.changes.changed() => {
                                changed.map_err(|_closed| PollError::InvalidPollable)?;
                            }
                        }
                        let generation = *item.changes.borrow_and_update();
                        if generation != item.observed_generation {
                            return Ok(ReadyPollable {
                                index: item.index,
                                pollable: item.pollable,
                                generation,
                            });
                        }
                    }
                }
                .boxed(),
            );
        }

        let mut shutdown = self.inner.shutdown.subscribe();
        let readiness = async {
            waits
                .next()
                .await
                .ok_or(PollError::Stopped)
                .and_then(std::convert::identity)
        };
        match timeout {
            Some(timeout) => {
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => Err(PollError::Stopped),
                    ready = readiness => ready,
                    () = tokio::time::sleep(timeout) => Err(PollError::TimedOut),
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => Err(PollError::Stopped),
                    ready = readiness => ready,
                }
            }
        }
    }

    /// Stop the generation and wake all worker waits.
    pub fn stop(&self) {
        if !self.inner.alive.swap(false, Ordering::AcqRel) {
            return;
        }
        self.inner.shutdown.send_replace(true);
        if let Ok(mut state) = self.inner.state.lock() {
            state.notifications.clear();
            state.pollables.clear();
        }
        self.inner.pending_actor_events.store(0, Ordering::Release);
    }

    /// Snapshot bounded resource and wait-registration counts.
    pub fn stats(&self) -> CoordinationStats {
        let (notifications, pollables) = self
            .inner
            .state
            .lock()
            .map(|state| (state.notifications.len(), state.pollables.len()))
            .unwrap_or_default();
        CoordinationStats {
            notifications,
            pollables,
            wait_registrations: self.inner.wait_registrations.load(Ordering::Acquire),
            pending_actor_events: self.inner.pending_actor_events.load(Ordering::Acquire),
        }
    }

    fn prepare_pollables(
        &self,
        pollables: &[PollableId],
    ) -> Result<Vec<PreparedPollable>, PollError> {
        self.ensure_poll_alive()?;
        if pollables.is_empty() {
            return Err(PollError::EmptyWaitSet);
        }
        if pollables.len() > self.inner.config.max_wait_set {
            return Err(PollError::LimitReached);
        }
        let mut unique = HashSet::with_capacity(pollables.len());
        let state = self.lock_poll_state()?;
        pollables
            .iter()
            .copied()
            .enumerate()
            .map(|(index, pollable)| {
                if !unique.insert(pollable) {
                    return Err(PollError::DuplicatePollable);
                }
                let target = state
                    .pollables
                    .get(&pollable)
                    .ok_or(PollError::InvalidPollable)?;
                let notification = state
                    .notifications
                    .get(&target.notification)
                    .ok_or(PollError::InvalidPollable)?;
                Ok(PreparedPollable {
                    index,
                    pollable,
                    observed_generation: target.observed_generation,
                    changes: notification.changes.subscribe(),
                    invalidated: target.invalidated.subscribe(),
                })
            })
            .collect()
    }

    fn choose_ready(
        &self,
        ready: &[ReadyPollable],
        wait_len: usize,
        mode: WaitMode,
    ) -> Option<ReadyPollable> {
        let fallback = ready.first().copied();
        if mode == WaitMode::Biased {
            return ready
                .iter()
                .copied()
                .min_by_key(|item| item.index)
                .or(fallback);
        }
        let start = usize::try_from(
            self.inner.fair_cursor.fetch_add(1, Ordering::Relaxed)
                % u64::try_from(wait_len).unwrap_or(1),
        )
        .unwrap_or(0);
        ready
            .iter()
            .copied()
            .min_by_key(|item| (item.index + wait_len - start) % wait_len)
            .or(fallback)
    }

    fn ensure_alive(&self) -> Result<(), NotificationError> {
        if self.inner.alive.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(NotificationError::Stopped)
        }
    }

    fn ensure_poll_alive(&self) -> Result<(), PollError> {
        self.ensure_alive().map_err(map_notification_to_poll_error)
    }

    fn lock_state(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, CoordinationState>, NotificationError> {
        self.inner
            .state
            .lock()
            .map_err(|_poisoned| NotificationError::Stopped)
    }

    fn lock_poll_state(&self) -> Result<std::sync::MutexGuard<'_, CoordinationState>, PollError> {
        self.inner
            .state
            .lock()
            .map_err(|_poisoned| PollError::Stopped)
    }
}

impl Drop for CoordinationHub {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.stop();
        }
    }
}

fn allocate_global_id(allocator: &AtomicU64) -> Result<u64, NotificationError> {
    allocator
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current != 0).then(|| current.checked_add(1)).flatten()
        })
        .map_err(|_exhausted| NotificationError::LimitReached)
}

const fn map_notification_to_poll_error(error: NotificationError) -> PollError {
    match error {
        NotificationError::Stopped => PollError::Stopped,
        NotificationError::LimitReached | NotificationError::BacklogFull => PollError::LimitReached,
        NotificationError::InvalidHandle
        | NotificationError::InvalidGeneration
        | NotificationError::AlreadyWatched => PollError::InvalidPollable,
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{Context, Result, ensure};

    use super::*;

    fn hub() -> Result<(CoordinationHub, mpsc::Receiver<NotificationId>)> {
        Ok(CoordinationHub::new(
            7,
            CoordinationConfig {
                max_notifications: 8,
                max_pollables: 16,
                max_wait_set: 4,
            },
        )?)
    }

    #[tokio::test]
    async fn actor_watch_is_lost_wakeup_safe_and_coalesced() -> Result<()> {
        let (hub, mut events) = hub()?;
        let notification = hub.create_notification()?;
        hub.watch_actor(notification, 0, 91)?;
        ensure!(hub.signal(notification, 1)? == 1);
        // Pull the ID into the runtime lane before later signals. The event
        // payload is materialized only at dispatch selection, so it still
        // reports the greatest coalesced generation.
        let queued = events.recv().await;
        ensure!(queued == Some(notification));
        ensure!(hub.signal(notification, 3)? == 3);
        ensure!(hub.signal(notification, 2)? == 3);
        ensure!(
            hub.take_actor_event(notification)?
                == Some(ActorNotification {
                    notification,
                    generation: 3,
                    token: 91,
                })
        );
        ensure!(events.try_recv().is_err());
        ensure!(hub.stats().pending_actor_events == 0);
        ensure!(hub.watch_actor(notification, 3, 92)? == ActorWatchResult::Armed);
        ensure!(hub.signal(notification, 3)? == 3);
        ensure!(hub.signal(notification, 2)? == 3);
        ensure!(events.try_recv().is_err());
        Ok(())
    }

    #[tokio::test]
    async fn signal_before_watch_queues_immediately() -> Result<()> {
        let (hub, mut events) = hub()?;
        let notification = hub.create_notification()?;
        ensure!(hub.signal(notification, 4)? == 4);
        ensure!(
            hub.watch_actor(notification, 0, 11)? == ActorWatchResult::Queued { generation: 4 }
        );
        ensure!(events.recv().await == Some(notification));
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_invalidates_queued_actor_events_without_failure() -> Result<()> {
        let (hub, mut events) = hub()?;
        let notification = hub.create_notification()?;
        hub.watch_actor(notification, 0, 5)?;
        hub.signal(notification, 1)?;
        ensure!(hub.drop_notification(notification) == Err(NotificationError::AlreadyWatched));
        let queued = events.recv().await.context("queued actor notification")?;
        hub.stop();
        ensure!(hub.take_actor_event(queued)? == None);
        ensure!(hub.stats().pending_actor_events == 0);
        Ok(())
    }

    #[tokio::test]
    async fn wait_any_is_bounded_fair_and_tears_down_registrations() -> Result<()> {
        let (hub, _events) = hub()?;
        let first = hub.create_notification()?;
        let second = hub.create_notification()?;
        let first_pollable = hub.notification_pollable(first, 0)?;
        let second_pollable = hub.notification_pollable(second, 0)?;

        let waiter = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.wait_any(
                    &[first_pollable, second_pollable],
                    Some(Duration::from_secs(1)),
                    WaitMode::Biased,
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        ensure!(hub.stats().wait_registrations == 2);
        ensure!(hub.signal(second, 5)? == 5);
        ensure!(
            waiter.await.context("join worker wait")?
                == Ok(ReadyPollable {
                    index: 1,
                    pollable: second_pollable,
                    generation: 5,
                })
        );
        ensure!(hub.stats().wait_registrations == 0);

        ensure!(hub.signal(first, 1)? == 1);
        let ready = hub.try_poll(&[first_pollable, second_pollable])?;
        ensure!(ready.len() == 2);
        let fair_one = hub
            .wait_any(&[first_pollable, second_pollable], None, WaitMode::Fair)
            .await?;
        let fair_two = hub
            .wait_any(&[first_pollable, second_pollable], None, WaitMode::Fair)
            .await?;
        ensure!(fair_one.index != fair_two.index);
        Ok(())
    }

    #[tokio::test]
    async fn dropped_wait_and_stop_release_registrations() -> Result<()> {
        let (hub, _events) = hub()?;
        let notification = hub.create_notification()?;
        let pollable = hub.notification_pollable(notification, 0)?;
        let task = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.wait_any(&[pollable], None, WaitMode::Biased).await })
        };
        tokio::task::yield_now().await;
        ensure!(hub.stats().wait_registrations == 1);
        task.abort();
        let _joined = task.await;
        ensure!(hub.stats().wait_registrations == 0);

        let task = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.wait_any(&[pollable], None, WaitMode::Biased).await })
        };
        tokio::task::yield_now().await;
        hub.stop();
        ensure!(task.await.context("join stopped wait")? == Err(PollError::Stopped));
        Ok(())
    }

    #[tokio::test]
    async fn dropping_pollable_or_notification_invalidates_active_wait() -> Result<()> {
        let (hub, _events) = hub()?;
        let first = hub.create_notification()?;
        let second = hub.create_notification()?;
        let first_pollable = hub.notification_pollable(first, 0)?;
        let second_pollable = hub.notification_pollable(second, 0)?;
        let task = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.wait_any(&[first_pollable, second_pollable], None, WaitMode::Biased)
                    .await
            })
        };
        tokio::task::yield_now().await;
        hub.drop_pollable(first_pollable)?;
        ensure!(
            task.await.context("join invalidated pollable wait")?
                == Err(PollError::InvalidPollable)
        );
        ensure!(hub.stats().wait_registrations == 0);

        let first_pollable = hub.notification_pollable(first, 0)?;
        let task = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.wait_any(&[first_pollable, second_pollable], None, WaitMode::Biased)
                    .await
            })
        };
        tokio::task::yield_now().await;
        hub.drop_notification(first)?;
        ensure!(
            task.await.context("join invalidated notification wait")?
                == Err(PollError::InvalidPollable)
        );
        ensure!(hub.stats().wait_registrations == 0);
        Ok(())
    }

    #[test]
    fn rejects_duplicate_over_limit_and_foreign_handles() -> Result<()> {
        let (hub, _events) = hub()?;
        let notification = hub.create_notification()?;
        let pollable = hub.notification_pollable(notification, 0)?;
        ensure!(hub.try_poll(&[pollable, pollable]) == Err(PollError::DuplicatePollable));
        ensure!(hub.try_poll(&[pollable; 5]) == Err(PollError::LimitReached));

        let (foreign, _foreign_events) = CoordinationHub::new(
            8,
            CoordinationConfig {
                max_notifications: 1,
                max_pollables: 1,
                max_wait_set: 1,
            },
        )?;
        ensure!(foreign.notification_pollable(notification, 0) == Err(PollError::InvalidPollable));
        Ok(())
    }
}
