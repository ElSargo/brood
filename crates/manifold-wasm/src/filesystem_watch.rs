//! Capability-rooted host filesystem watches shared by one actor generation.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{
    completion_io::FilesystemGrant,
    coordination::{CoordinationHub, NotificationError, NotificationId},
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FilesystemWatchId(u64);

impl FilesystemWatchId {
    pub(super) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(super) const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemWatchEventKind {
    Created,
    Modified,
    Removed,
    Changed,
    Rescan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemWatchBatchEvent {
    pub kind: FilesystemWatchEventKind,
    /// The relative path in the host platform's lossless native encoding.
    /// Unix stores raw `OsStr` bytes; Windows stores little-endian UTF-16 code
    /// units so even unpaired surrogates retain their identity.
    pub path: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum FilesystemWatchError {
    #[error("actor has stopped")]
    Stopped,
    #[error("filesystem watch limit reached")]
    Limit,
    #[error("invalid filesystem watch handle or path")]
    Invalid,
    #[error("filesystem watch capability is unavailable")]
    NotPermitted,
    #[error("filesystem watch has no pending events")]
    NotReady,
    #[error("filesystem watcher backend is unavailable: {0}")]
    Backend(String),
    #[error("filesystem watch state is unavailable")]
    Unavailable,
}

#[derive(Clone)]
pub struct FilesystemWatchHub {
    inner: Arc<FilesystemWatchInner>,
}

/// Daemon-owned native watcher registry shared by actor generations.
#[derive(Clone, Default)]
pub struct NativeFilesystemWatcherService {
    inner: Arc<NativeWatcherInner>,
}

impl std::fmt::Debug for NativeFilesystemWatcherService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeFilesystemWatcherService")
            .field("stats", &self.stats())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeFilesystemWatcherStats {
    pub registrations: usize,
    pub subscriptions: usize,
    pub setup_tasks: usize,
    pub overflows: u64,
    pub rescans: u64,
}

struct NativeWatcherInner {
    next_subscription: AtomicU64,
    next_setup: AtomicU64,
    registrations: Mutex<HashMap<NativeWatchKey, NativeRegistration>>,
    setup_threads: Mutex<Vec<JoinHandle<()>>>,
    pending_setups: Mutex<HashSet<u64>>,
    overflows: Arc<AtomicU64>,
    rescans: Arc<AtomicU64>,
}

impl Default for NativeWatcherInner {
    fn default() -> Self {
        Self {
            next_subscription: AtomicU64::new(1),
            next_setup: AtomicU64::new(1),
            registrations: Mutex::new(HashMap::new()),
            setup_threads: Mutex::new(Vec::new()),
            pending_setups: Mutex::new(HashSet::new()),
            overflows: Arc::new(AtomicU64::new(0)),
            rescans: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct NativeWatchKey {
    path: PathBuf,
    mode: RecursiveMode,
}

struct NativeRegistration {
    watcher: Option<RecommendedWatcher>,
    setup_complete: bool,
    subscribers: HashMap<u64, Weak<NativeSubscription>>,
    setup_id: u64,
}

struct NativeSubscription {
    alive: AtomicBool,
    watched_path: PathBuf,
    mode: RecursiveMode,
    projection_root: PathBuf,
    coordination: CoordinationHub,
    notification: NotificationId,
    queue: Arc<Mutex<WatchQueue>>,
    max_events: usize,
    generation_overflows: Arc<AtomicU64>,
    generation_rescans: Arc<AtomicU64>,
    service_overflows: Arc<AtomicU64>,
    service_rescans: Arc<AtomicU64>,
}

struct NativeSubscriptionToken {
    service: NativeFilesystemWatcherService,
    key: NativeWatchKey,
    id: u64,
    setup_id: u64,
    subscription: Arc<NativeSubscription>,
}

struct FilesystemWatchInner {
    root: Option<PathBuf>,
    coordination: CoordinationHub,
    next_watch: AtomicU64,
    alive: AtomicBool,
    max_watches: usize,
    max_events: usize,
    service: NativeFilesystemWatcherService,
    watches: Mutex<HashMap<FilesystemWatchId, FilesystemWatchEntry>>,
    orphaned_setups: Mutex<Vec<u64>>,
    overflows: Arc<AtomicU64>,
    rescans: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FilesystemWatchStats {
    pub watches: usize,
    pub max_watches: usize,
    pub max_events: usize,
    pub overflows: u64,
    pub rescans: u64,
}

struct FilesystemWatchEntry {
    notification: NotificationId,
    queue: Arc<Mutex<WatchQueue>>,
    subscription: NativeSubscriptionToken,
}

#[derive(Default)]
struct WatchQueue {
    generation: u64,
    events: VecDeque<WatchEvent>,
}

struct WatchEvent {
    generation: u64,
    event: FilesystemWatchBatchEvent,
}

impl WatchEvent {
    fn encoded_len(&self) -> usize {
        event_label_text(self.event.kind)
            .len()
            .saturating_add(1)
            .saturating_add(preview1_encoded_path_len(&self.event.path))
            .saturating_add(1)
    }

    fn encode_into(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(event_label_text(self.event.kind).as_bytes());
        bytes.push(b'\t');
        encode_preview1_path(&self.event.path, bytes);
        bytes.push(b'\n');
    }
}

impl NativeFilesystemWatcherService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> NativeFilesystemWatcherStats {
        self.reap_finished_setup_threads();
        let Ok(registrations) = self.inner.registrations.lock() else {
            return NativeFilesystemWatcherStats::default();
        };
        NativeFilesystemWatcherStats {
            registrations: registrations.len(),
            subscriptions: registrations
                .values()
                .map(|registration| registration.subscribers.len())
                .sum(),
            setup_tasks: self
                .inner
                .pending_setups
                .lock()
                .map_or(0, |setups| setups.len()),
            overflows: self.inner.overflows.load(Ordering::Acquire),
            rescans: self.inner.rescans.load(Ordering::Acquire),
        }
    }

    fn subscribe(
        &self,
        watched_path: PathBuf,
        mode: RecursiveMode,
        subscription: Arc<NativeSubscription>,
    ) -> Result<NativeSubscriptionToken, FilesystemWatchError> {
        self.reap_finished_setup_threads();
        let id = self
            .inner
            .next_subscription
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
            .map_err(|_exhausted| FilesystemWatchError::Limit)?;
        let key = NativeWatchKey {
            path: watched_path,
            mode,
        };
        let mut registrations = self
            .inner
            .registrations
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)?;
        if let Some(registration) = registrations.get_mut(&key) {
            registration
                .subscribers
                .retain(|_id, subscriber| subscriber.strong_count() > 0);
            registration
                .subscribers
                .insert(id, Arc::downgrade(&subscription));
            let setup_complete = registration.setup_complete;
            let setup_id = registration.setup_id;
            drop(registrations);
            if setup_complete {
                signal_rescan(&subscription);
            }
            return Ok(NativeSubscriptionToken {
                service: self.clone(),
                key,
                id,
                setup_id,
                subscription,
            });
        }

        let setup_id = self
            .inner
            .next_setup
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
            .map_err(|_exhausted| FilesystemWatchError::Limit)?;

        let callback_inner = Arc::downgrade(&self.inner);
        let callback_key = key.clone();
        let watcher = notify::recommended_watcher(move |result| {
            route_native_event(&callback_inner, &callback_key, result);
        })
        .map_err(|error| FilesystemWatchError::Backend(error.to_string()))?;
        registrations.insert(
            key.clone(),
            NativeRegistration {
                watcher: None,
                setup_complete: false,
                subscribers: HashMap::from([(id, Arc::downgrade(&subscription))]),
                setup_id,
            },
        );
        drop(registrations);

        let Ok(mut pending_setups) = self.inner.pending_setups.lock() else {
            self.remove_registration(&key);
            return Err(FilesystemWatchError::Unavailable);
        };
        pending_setups.insert(setup_id);
        drop(pending_setups);

        let setup_inner = Arc::clone(&self.inner);
        let setup_key = key.clone();
        let setup = thread::Builder::new()
            .name("manifold-native-fs-watch-setup".to_string())
            .spawn(move || {
                finish_native_watch_setup(Arc::clone(&setup_inner), setup_key, setup_id, watcher);
                if let Ok(mut pending) = setup_inner.pending_setups.lock() {
                    pending.remove(&setup_id);
                }
            })
            .map_err(|error| {
                self.remove_registration(&key);
                if let Ok(mut pending) = self.inner.pending_setups.lock() {
                    pending.remove(&setup_id);
                }
                FilesystemWatchError::Backend(error.to_string())
            })?;
        let Ok(mut setup_threads) = self.inner.setup_threads.lock() else {
            self.remove_registration(&key);
            // Dropping a JoinHandle detaches the setup. It still owns its
            // watcher until completion, observes the removed registration,
            // and clears its pending setup ID without blocking this call.
            drop(setup);
            return Err(FilesystemWatchError::Unavailable);
        };
        setup_threads.push(setup);
        drop(setup_threads);
        Ok(NativeSubscriptionToken {
            service: self.clone(),
            key,
            id,
            setup_id,
            subscription,
        })
    }

    fn unsubscribe(&self, key: &NativeWatchKey, id: u64) {
        let Ok(mut registrations) = self.inner.registrations.lock() else {
            return;
        };
        let remove_registration = registrations.get_mut(key).is_some_and(|registration| {
            registration.subscribers.remove(&id);
            registration
                .subscribers
                .retain(|_id, subscriber| subscriber.strong_count() > 0);
            registration.subscribers.is_empty()
        });
        let removed = remove_registration
            .then(|| registrations.remove(key))
            .flatten();
        drop(registrations);
        // `RecommendedWatcher::drop` may join a callback which needs the
        // registry mutex. Never destroy a backend while holding that mutex.
        drop(removed);
    }

    fn remove_registration(&self, key: &NativeWatchKey) {
        let removed = self
            .inner
            .registrations
            .lock()
            .ok()
            .and_then(|mut registrations| registrations.remove(key));
        drop(removed);
    }

    fn reap_finished_setup_threads(&self) {
        let Ok(mut setup_threads) = self.inner.setup_threads.lock() else {
            return;
        };
        let finished = setup_threads
            .extract_if(.., |thread| thread.is_finished())
            .collect::<Vec<_>>();
        drop(setup_threads);
        for thread in finished {
            let _join_result = thread.join();
        }
    }

    fn setup_pending(&self, setup_id: u64) -> bool {
        self.inner
            .pending_setups
            .lock()
            .is_ok_and(|pending| pending.contains(&setup_id))
    }
}

impl Drop for NativeSubscriptionToken {
    fn drop(&mut self) {
        self.subscription.alive.store(false, Ordering::Release);
        self.service.unsubscribe(&self.key, self.id);
    }
}

impl FilesystemWatchHub {
    pub(super) fn new_with_service(
        service: NativeFilesystemWatcherService,
        filesystem: Option<&FilesystemGrant>,
        coordination: CoordinationHub,
        max_watches: usize,
        max_events: usize,
    ) -> Result<Self, FilesystemWatchError> {
        if max_watches == 0 || max_events == 0 {
            return Err(FilesystemWatchError::Limit);
        }
        let root = filesystem
            .map(|grant| std::fs::canonicalize(&grant.host_path))
            .transpose()
            .map_err(|error| FilesystemWatchError::Backend(error.to_string()))?;
        Ok(Self {
            inner: Arc::new(FilesystemWatchInner {
                root,
                coordination,
                next_watch: AtomicU64::new(1),
                alive: AtomicBool::new(true),
                max_watches,
                max_events,
                service,
                watches: Mutex::new(HashMap::new()),
                orphaned_setups: Mutex::new(Vec::new()),
                overflows: Arc::new(AtomicU64::new(0)),
                rescans: Arc::new(AtomicU64::new(0)),
            }),
        })
    }

    pub(super) fn create(
        &self,
        guest_path: &Path,
        recursive: bool,
    ) -> Result<(FilesystemWatchId, NotificationId), FilesystemWatchError> {
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err(FilesystemWatchError::Stopped);
        }
        let root = self
            .inner
            .root
            .as_ref()
            .ok_or(FilesystemWatchError::NotPermitted)?;
        let relative = validate_relative_path(guest_path)?;
        let watched_path = std::fs::canonicalize(root.join(relative))
            .map_err(|_error| FilesystemWatchError::Invalid)?;
        if !watched_path.starts_with(root) {
            return Err(FilesystemWatchError::NotPermitted);
        }

        let mut watches = self
            .inner
            .watches
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)?;
        let mut orphaned_setups = self
            .inner
            .orphaned_setups
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)?;
        orphaned_setups.retain(|setup| self.inner.service.setup_pending(*setup));
        if watches.len().saturating_add(orphaned_setups.len()) >= self.inner.max_watches {
            return Err(FilesystemWatchError::Limit);
        }
        drop(orphaned_setups);
        let raw = self
            .inner
            .next_watch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
            .map_err(|_exhausted| FilesystemWatchError::Limit)?;
        let id = FilesystemWatchId(raw);
        let notification = self
            .inner
            .coordination
            .create_notification()
            .map_err(map_notification_error)?;
        let queue = Arc::new(Mutex::new(WatchQueue::default()));
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let subscription = Arc::new(NativeSubscription {
            alive: AtomicBool::new(true),
            watched_path: watched_path.clone(),
            mode,
            projection_root: root.clone(),
            coordination: self.inner.coordination.clone(),
            notification,
            queue: Arc::clone(&queue),
            max_events: self.inner.max_events,
            generation_overflows: Arc::clone(&self.inner.overflows),
            generation_rescans: Arc::clone(&self.inner.rescans),
            service_overflows: Arc::clone(&self.inner.service.inner.overflows),
            service_rescans: Arc::clone(&self.inner.service.inner.rescans),
        });
        let subscription = match self
            .inner
            .service
            .subscribe(watched_path, mode, subscription)
        {
            Ok(subscription) => subscription,
            Err(error) => {
                let _drop_result = self.inner.coordination.retire_notification(notification);
                return Err(error);
            }
        };
        watches.insert(
            id,
            FilesystemWatchEntry {
                notification,
                queue,
                subscription,
            },
        );
        drop(watches);
        Ok((id, notification))
    }

    pub(super) fn notification(
        &self,
        watch: FilesystemWatchId,
    ) -> Result<NotificationId, FilesystemWatchError> {
        self.with_entry(watch, |entry| entry.notification)
    }

    pub(super) fn pending_bytes(
        &self,
        watch: FilesystemWatchId,
    ) -> Result<usize, FilesystemWatchError> {
        self.with_entry(watch, |entry| Arc::clone(&entry.queue))?
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)
            .map(|queue| queue.events.iter().map(WatchEvent::encoded_len).sum())
    }

    pub(super) fn read(
        &self,
        watch: FilesystemWatchId,
        capacity: usize,
    ) -> Result<(u64, Vec<u8>), FilesystemWatchError> {
        let queue = self.with_entry(watch, |entry| Arc::clone(&entry.queue))?;
        let mut queue = queue
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)?;
        if queue.events.is_empty() {
            return Err(FilesystemWatchError::NotReady);
        }
        let mut bytes = Vec::with_capacity(capacity);
        let mut generation = 0;
        while let Some(event) = queue.events.front() {
            if bytes.len().saturating_add(event.encoded_len()) > capacity {
                break;
            }
            let Some(event) = queue.events.pop_front() else {
                break;
            };
            generation = event.generation;
            event.encode_into(&mut bytes);
        }
        if bytes.is_empty() {
            return Err(FilesystemWatchError::Limit);
        }
        drop(queue);
        Ok((generation, bytes))
    }

    pub(super) fn read_typed(
        &self,
        watch: FilesystemWatchId,
    ) -> Result<(u64, Vec<FilesystemWatchBatchEvent>), FilesystemWatchError> {
        let queue = self.with_entry(watch, |entry| Arc::clone(&entry.queue))?;
        let mut queue = queue
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)?;
        if queue.events.is_empty() {
            return Err(FilesystemWatchError::NotReady);
        }
        let generation = queue
            .events
            .back()
            .map_or(queue.generation, |event| event.generation);
        let events = queue.events.drain(..).map(|event| event.event).collect();
        drop(queue);
        Ok((generation, events))
    }

    pub(super) fn drop_watch(&self, watch: FilesystemWatchId) -> Result<(), FilesystemWatchError> {
        let mut watches = self
            .inner
            .watches
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)?;
        let entry = watches.get(&watch).ok_or(FilesystemWatchError::Invalid)?;
        self.inner
            .coordination
            .retire_notification(entry.notification)
            .map_err(map_notification_error)?;
        entry
            .subscription
            .subscription
            .alive
            .store(false, Ordering::Release);
        let entry = watches
            .remove(&watch)
            .ok_or(FilesystemWatchError::Invalid)?;
        drop(watches);
        if self
            .inner
            .service
            .setup_pending(entry.subscription.setup_id)
        {
            self.inner
                .orphaned_setups
                .lock()
                .map_err(|_poisoned| FilesystemWatchError::Unavailable)?
                .push(entry.subscription.setup_id);
        }
        drop(entry);
        Ok(())
    }

    pub(super) fn stop(&self) {
        self.inner.alive.store(false, Ordering::Release);
        if let Ok(mut watches) = self.inner.watches.lock() {
            let drained = watches
                .drain()
                .map(|(_id, entry)| entry)
                .collect::<Vec<_>>();
            drop(watches);
            for entry in drained {
                entry
                    .subscription
                    .subscription
                    .alive
                    .store(false, Ordering::Release);
                drop(entry.subscription);
                let _drop_result = self
                    .inner
                    .coordination
                    .retire_notification(entry.notification);
            }
        }
    }

    pub fn stats(&self) -> FilesystemWatchStats {
        FilesystemWatchStats {
            watches: self.inner.watches.lock().map_or(0, |watches| watches.len()),
            max_watches: self.inner.max_watches,
            max_events: self.inner.max_events,
            overflows: self.inner.overflows.load(Ordering::Acquire),
            rescans: self.inner.rescans.load(Ordering::Acquire),
        }
    }

    fn with_entry<T>(
        &self,
        watch: FilesystemWatchId,
        function: impl FnOnce(&FilesystemWatchEntry) -> T,
    ) -> Result<T, FilesystemWatchError> {
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err(FilesystemWatchError::Stopped);
        }
        self.inner
            .watches
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)?
            .get(&watch)
            .map(function)
            .ok_or(FilesystemWatchError::Invalid)
    }
}

fn finish_native_watch_setup(
    inner: Arc<NativeWatcherInner>,
    key: NativeWatchKey,
    setup_id: u64,
    mut watcher: RecommendedWatcher,
) {
    let result = watcher.watch(&key.path, key.mode);
    install_native_watch_result(&inner, &key, setup_id, watcher, result.is_ok());
}

fn install_native_watch_result(
    inner: &Arc<NativeWatcherInner>,
    key: &NativeWatchKey,
    setup_id: u64,
    watcher: RecommendedWatcher,
    succeeded: bool,
) {
    let Ok(mut registrations) = inner.registrations.lock() else {
        return;
    };
    let Some(registration) = registrations.get_mut(key) else {
        drop(registrations);
        drop(watcher);
        return;
    };
    if registration.setup_id != setup_id {
        drop(registrations);
        drop(watcher);
        return;
    }
    let (replaced, failed) = if succeeded {
        (registration.watcher.replace(watcher), None)
    } else {
        (None, Some(watcher))
    };
    registration.setup_complete = true;
    let subscriptions = registration
        .subscribers
        .values()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    drop(registrations);
    // Defensive even though a matching setup should install only once.
    drop(replaced);
    drop(failed);
    if let Ok(mut pending) = inner.pending_setups.lock() {
        pending.remove(&setup_id);
    }
    for subscription in subscriptions {
        signal_rescan(&subscription);
    }
}

fn route_native_event(
    inner: &Weak<NativeWatcherInner>,
    key: &NativeWatchKey,
    result: notify::Result<Event>,
) {
    let Some(inner) = inner.upgrade() else {
        return;
    };
    let Ok(registrations) = inner.registrations.lock() else {
        return;
    };
    let Some(registration) = registrations.get(key) else {
        return;
    };
    let subscriptions = registration
        .subscribers
        .values()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    drop(registrations);
    for subscription in subscriptions {
        record_subscription_event(&subscription, &result);
    }
}

fn signal_rescan(subscription: &NativeSubscription) {
    if !subscription.alive.load(Ordering::Acquire) {
        return;
    }
    let Ok(mut queue) = subscription.queue.lock() else {
        return;
    };
    queue.generation = queue.generation.saturating_add(1).max(1);
    let generation = queue.generation;
    increment_saturating(&subscription.generation_rescans);
    increment_saturating(&subscription.service_rescans);
    push_overflow(&mut queue.events, generation);
    drop(queue);
    let _signal_result = subscription
        .coordination
        .signal(subscription.notification, generation);
}

fn record_subscription_event(subscription: &NativeSubscription, result: &notify::Result<Event>) {
    if !subscription.alive.load(Ordering::Acquire) {
        return;
    }
    let Ok(mut queue) = subscription.queue.lock() else {
        return;
    };
    match result {
        Ok(event) => {
            let Some(kind) = event_kind(event.kind) else {
                return;
            };
            queue.generation = queue.generation.saturating_add(1).max(1);
            let generation = queue.generation;
            for path in &event.paths {
                let in_scope = match subscription.mode {
                    RecursiveMode::Recursive => path.starts_with(&subscription.watched_path),
                    RecursiveMode::NonRecursive => {
                        path == &subscription.watched_path
                            || path.parent() == Some(subscription.watched_path.as_path())
                    }
                };
                if !in_scope {
                    continue;
                }
                let Ok(path) = path.strip_prefix(&subscription.projection_root) else {
                    continue;
                };
                push_subscription_event(&mut queue.events, subscription, generation, kind, path);
            }
        }
        Err(_error) => {
            queue.generation = queue.generation.saturating_add(1).max(1);
            let generation = queue.generation;
            record_overflow(subscription);
            push_overflow(&mut queue.events, generation);
        }
    }
    if queue.events.is_empty() {
        return;
    }
    let generation = queue.generation;
    drop(queue);
    let _signal_result = subscription
        .coordination
        .signal(subscription.notification, generation);
}

fn push_bounded_event(
    events: &mut VecDeque<WatchEvent>,
    max_events: usize,
    generation: u64,
    kind: FilesystemWatchEventKind,
    path: &Path,
) {
    if events.len() >= max_events {
        push_overflow(events, generation);
        return;
    }
    let path = platform_path_bytes(path);
    events.push_back(WatchEvent {
        generation,
        event: FilesystemWatchBatchEvent { kind, path },
    });
}

fn push_subscription_event(
    events: &mut VecDeque<WatchEvent>,
    subscription: &NativeSubscription,
    generation: u64,
    kind: FilesystemWatchEventKind,
    path: &Path,
) {
    if events.len() >= subscription.max_events {
        record_overflow(subscription);
    }
    push_bounded_event(events, subscription.max_events, generation, kind, path);
}

fn record_overflow(subscription: &NativeSubscription) {
    increment_saturating(&subscription.generation_overflows);
    increment_saturating(&subscription.generation_rescans);
    increment_saturating(&subscription.service_overflows);
    increment_saturating(&subscription.service_rescans);
}

fn increment_saturating(counter: &AtomicU64) {
    let _previous = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_add(1))
    });
}

fn push_overflow(events: &mut VecDeque<WatchEvent>, generation: u64) {
    events.clear();
    events.push_back(WatchEvent {
        generation,
        event: FilesystemWatchBatchEvent {
            kind: FilesystemWatchEventKind::Rescan,
            path: b".".to_vec(),
        },
    });
}

/// Encode an opaque path into the legacy line-oriented Preview 1 batch. ASCII
/// graphic bytes remain readable except `%`, which is the escape introducer;
/// every other byte is represented as `%HH`. This makes tabs, newlines,
/// invalid UTF-8 and literal percent sequences unambiguous and reversible.
fn encode_preview1_path(path: &[u8], output: &mut Vec<u8>) {
    for &byte in path {
        if (0x20..=0x7e).contains(&byte) && byte != b'%' {
            output.push(byte);
        } else {
            output.push(b'%');
            output.push(hex_digit(byte >> 4));
            output.push(hex_digit(byte & 0x0f));
        }
    }
}

const fn hex_digit(nibble: u8) -> u8 {
    if nibble < 10 {
        b'0' + nibble
    } else {
        b'A' + nibble - 10
    }
}

fn preview1_encoded_path_len(path: &[u8]) -> usize {
    path.iter().fold(0_usize, |length, &byte| {
        length.saturating_add(if (0x20..=0x7e).contains(&byte) && byte != b'%' {
            1
        } else {
            3
        })
    })
}

#[cfg(unix)]
fn platform_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn platform_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

const fn event_kind(kind: EventKind) -> Option<FilesystemWatchEventKind> {
    match kind {
        EventKind::Create(_) => Some(FilesystemWatchEventKind::Created),
        EventKind::Modify(_) => Some(FilesystemWatchEventKind::Modified),
        EventKind::Remove(_) => Some(FilesystemWatchEventKind::Removed),
        EventKind::Access(_) => None,
        EventKind::Other | EventKind::Any => Some(FilesystemWatchEventKind::Changed),
    }
}

const fn event_label_text(kind: FilesystemWatchEventKind) -> &'static str {
    match kind {
        FilesystemWatchEventKind::Created => "created",
        FilesystemWatchEventKind::Modified => "modified",
        FilesystemWatchEventKind::Removed => "removed",
        FilesystemWatchEventKind::Changed => "changed",
        FilesystemWatchEventKind::Rescan => "rescan",
    }
}

fn validate_relative_path(path: &Path) -> Result<&Path, FilesystemWatchError> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(FilesystemWatchError::Invalid);
    }
    Ok(path)
}

const fn map_notification_error(error: NotificationError) -> FilesystemWatchError {
    match error {
        NotificationError::LimitReached => FilesystemWatchError::Limit,
        NotificationError::Stopped => FilesystemWatchError::Stopped,
        NotificationError::InvalidHandle
        | NotificationError::InvalidGeneration
        | NotificationError::AlreadyWatched => FilesystemWatchError::Invalid,
        NotificationError::BacklogFull => FilesystemWatchError::Unavailable,
    }
}
