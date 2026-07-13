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
    pub path: String,
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
}

struct NativeWatcherInner {
    next_subscription: AtomicU64,
    next_setup: AtomicU64,
    registrations: Mutex<HashMap<NativeWatchKey, NativeRegistration>>,
    setup_threads: Mutex<Vec<JoinHandle<()>>>,
    pending_setups: Mutex<HashSet<u64>>,
}

impl Default for NativeWatcherInner {
    fn default() -> Self {
        Self {
            next_subscription: AtomicU64::new(1),
            next_setup: AtomicU64::new(1),
            registrations: Mutex::new(HashMap::new()),
            setup_threads: Mutex::new(Vec::new()),
            pending_setups: Mutex::new(HashSet::new()),
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
    #[cfg(test)]
    _drop_probe: Option<RegistrationDropProbe>,
}

#[cfg(test)]
struct RegistrationDropProbe {
    inner: Weak<NativeWatcherInner>,
    registry_was_unlocked: Arc<AtomicBool>,
}

#[cfg(test)]
impl Drop for RegistrationDropProbe {
    fn drop(&mut self) {
        let unlocked = self
            .inner
            .upgrade()
            .is_some_and(|inner| inner.registrations.try_lock().is_ok());
        self.registry_was_unlocked
            .store(unlocked, Ordering::Release);
    }
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
    const fn encoded_len(&self) -> usize {
        event_label_text(self.event.kind)
            .len()
            .saturating_add(1)
            .saturating_add(self.event.path.len())
            .saturating_add(1)
    }

    fn encode_into(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(event_label_text(self.event.kind).as_bytes());
        bytes.push(b'\t');
        bytes.extend_from_slice(self.event.path.as_bytes());
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
                #[cfg(test)]
                _drop_probe: None,
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

    #[cfg(test)]
    fn emit_for_test(&self, path: &Path, mode: RecursiveMode, event: Event) {
        let key = NativeWatchKey {
            path: path.to_path_buf(),
            mode,
        };
        route_native_event(&Arc::downgrade(&self.inner), &key, Ok(event));
    }
}

impl Drop for NativeSubscriptionToken {
    fn drop(&mut self) {
        self.subscription.alive.store(false, Ordering::Release);
        self.service.unsubscribe(&self.key, self.id);
    }
}

impl FilesystemWatchHub {
    #[cfg(test)]
    pub(super) fn new(
        filesystem: Option<&FilesystemGrant>,
        coordination: CoordinationHub,
        max_watches: usize,
        max_events: usize,
    ) -> Result<Self, FilesystemWatchError> {
        Self::new_with_service(
            NativeFilesystemWatcherService::new(),
            filesystem,
            coordination,
            max_watches,
            max_events,
        )
    }

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

    #[cfg(test)]
    fn setup_thread_count(&self) -> usize {
        self.inner.service.stats().setup_tasks
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
                push_bounded_event(
                    &mut queue.events,
                    subscription.max_events,
                    generation,
                    kind,
                    path,
                );
            }
        }
        Err(_error) => {
            queue.generation = queue.generation.saturating_add(1).max(1);
            let generation = queue.generation;
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
    let path = path.to_string_lossy().into_owned();
    events.push_back(WatchEvent {
        generation,
        event: FilesystemWatchBatchEvent { kind, path },
    });
}

fn push_overflow(events: &mut VecDeque<WatchEvent>, generation: u64) {
    events.clear();
    events.push_back(WatchEvent {
        generation,
        event: FilesystemWatchBatchEvent {
            kind: FilesystemWatchEventKind::Rescan,
            path: ".".to_string(),
        },
    });
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

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use anyhow::{Context, Result, anyhow, ensure};
    use notify::{Event, EventKind, RecursiveMode, event::CreateKind};

    use super::{FilesystemWatchError, FilesystemWatchHub, NativeFilesystemWatcherService};
    use crate::{
        completion_io::FilesystemGrant,
        coordination::{ActorWatchResult, CoordinationConfig, CoordinationHub, WaitMode},
    };

    async fn await_setup(
        watches: &FilesystemWatchHub,
        coordination: &CoordinationHub,
        watch: super::FilesystemWatchId,
        notification: crate::coordination::NotificationId,
    ) -> Result<u64> {
        let pollable = coordination.notification_pollable(notification, 0)?;
        let ready = tokio::time::timeout(
            Duration::from_secs(5),
            coordination.wait_any(&[pollable], None, WaitMode::Biased),
        )
        .await??;
        ensure!(ready.pollable == pollable);
        let bytes = watches.pending_bytes(watch)?;
        let (generation, events) = watches.read(watch, bytes)?;
        ensure!(String::from_utf8(events)?.contains("rescan\t."));
        coordination.drop_pollable(pollable)?;
        Ok(generation)
    }

    #[tokio::test]
    async fn native_watch_wakes_worker_pollable_and_returns_relative_path() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (coordination, _actor_events) = CoordinationHub::new(
            1,
            CoordinationConfig {
                max_notifications: 4,
                max_pollables: 4,
                max_wait_set: 4,
            },
        )?;
        let watches = FilesystemWatchHub::new(
            Some(&FilesystemGrant {
                host_path: root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: true,
            }),
            coordination.clone(),
            2,
            32,
        )?;
        let (watch, notification) = watches.create(Path::new("."), true)?;
        let setup_pollable = coordination.notification_pollable(notification, 0)?;

        let setup_ready = tokio::time::timeout(
            Duration::from_secs(5),
            coordination.wait_any(&[setup_pollable], None, WaitMode::Biased),
        )
        .await??;
        ensure!(setup_ready.pollable == setup_pollable);
        let setup_bytes = watches.pending_bytes(watch)?;
        let (setup_generation, setup_events) = watches.read(watch, setup_bytes)?;
        ensure!(String::from_utf8(setup_events)?.contains("rescan\t."));
        coordination.drop_pollable(setup_pollable)?;

        let pollable = coordination.notification_pollable(notification, setup_generation)?;

        fs::write(root.path().join("created.txt"), b"created")?;
        let ready = tokio::time::timeout(
            Duration::from_secs(5),
            coordination.wait_any(&[pollable], None, WaitMode::Biased),
        )
        .await??;
        ensure!(ready.pollable == pollable);
        let bytes = watches.pending_bytes(watch)?;
        ensure!(bytes > 0);
        let (generation, events) = watches.read(watch, bytes)?;
        ensure!(generation > 0);
        let events = String::from_utf8(events)?;
        ensure!(events.contains("created.txt"), "events were {events:?}");

        coordination.drop_pollable(pollable)?;
        watches.stop();
        coordination.stop();
        Ok(())
    }

    #[test]
    fn watch_path_cannot_escape_capability_root() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (coordination, _actor_events) = CoordinationHub::new(
            1,
            CoordinationConfig {
                max_notifications: 2,
                max_pollables: 2,
                max_wait_set: 2,
            },
        )?;
        let watches = FilesystemWatchHub::new(
            Some(&FilesystemGrant {
                host_path: root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: false,
            }),
            coordination,
            1,
            8,
        )?;
        ensure!(matches!(
            watches.create(Path::new("../outside"), true),
            Err(FilesystemWatchError::Invalid)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn repeated_create_drop_reaps_setup_threads_and_notifications() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (coordination, _actor_events) = CoordinationHub::new(
            1,
            CoordinationConfig {
                max_notifications: 2,
                max_pollables: 2,
                max_wait_set: 2,
            },
        )?;
        let watches = FilesystemWatchHub::new(
            Some(&FilesystemGrant {
                host_path: root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: true,
            }),
            coordination.clone(),
            2,
            8,
        )?;

        for _cycle in 0..32 {
            let (watch, notification) = watches.create(Path::new("."), false)?;
            let _generation = await_setup(&watches, &coordination, watch, notification).await?;
            watches.drop_watch(watch)?;
            ensure!(watches.setup_thread_count() == 0);
            ensure!(coordination.stats().notifications == 0);
        }

        watches.stop();
        coordination.stop();
        Ok(())
    }

    #[tokio::test]
    async fn drop_watch_retires_notification_with_queued_actor_event() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (coordination, mut actor_events) = CoordinationHub::new(
            1,
            CoordinationConfig {
                max_notifications: 2,
                max_pollables: 2,
                max_wait_set: 2,
            },
        )?;
        let watches = FilesystemWatchHub::new(
            Some(&FilesystemGrant {
                host_path: root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: true,
            }),
            coordination.clone(),
            1,
            8,
        )?;
        let (watch, notification) = watches.create(Path::new("."), false)?;
        let setup_generation = await_setup(&watches, &coordination, watch, notification).await?;
        ensure!(
            coordination.watch_actor(notification, 0, 91)?
                == ActorWatchResult::Queued {
                    generation: setup_generation,
                }
        );
        ensure!(coordination.stats().pending_actor_events == 1);

        watches.drop_watch(watch)?;
        ensure!(coordination.stats().notifications == 0);
        ensure!(coordination.stats().pending_actor_events == 0);
        let queued = actor_events
            .recv()
            .await
            .context("queued notification ID")?;
        ensure!(queued == notification);
        ensure!(coordination.take_actor_event(queued)? == None);
        ensure!(coordination.stats().pending_actor_events == 0);

        watches.stop();
        coordination.stop();
        Ok(())
    }

    #[tokio::test]
    async fn shared_service_deduplicates_native_registration_and_refcounts_subscriptions()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let service = NativeFilesystemWatcherService::new();
        let (coordination, _actor_events) =
            CoordinationHub::new(11, CoordinationConfig::default())?;
        let watches = FilesystemWatchHub::new_with_service(
            service.clone(),
            Some(&FilesystemGrant {
                host_path: root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: false,
            }),
            coordination.clone(),
            4,
            8,
        )?;
        let (first, first_notification) = watches.create(Path::new("."), true)?;
        await_setup(&watches, &coordination, first, first_notification).await?;
        let (second, second_notification) = watches.create(Path::new("."), true)?;
        await_setup(&watches, &coordination, second, second_notification).await?;
        ensure!(
            service.stats().registrations == 1 && service.stats().subscriptions == 2,
            "same native path was not deduplicated: {:?}",
            service.stats()
        );

        watches.drop_watch(first)?;
        ensure!(
            service.stats().registrations == 1 && service.stats().subscriptions == 1,
            "dropping one logical handle released the shared backend: {:?}",
            service.stats()
        );
        watches.drop_watch(second)?;
        ensure!(service.stats().registrations == 0 && service.stats().subscriptions == 0);
        coordination.stop();
        Ok(())
    }

    #[tokio::test]
    async fn shared_registration_keeps_logical_overflow_queues_independent() -> Result<()> {
        let root = tempfile::tempdir()?;
        let service = NativeFilesystemWatcherService::new();
        let (small_coordination, _small_events) =
            CoordinationHub::new(21, CoordinationConfig::default())?;
        let (large_coordination, _large_events) =
            CoordinationHub::new(22, CoordinationConfig::default())?;
        let grant = FilesystemGrant {
            host_path: root.path().to_path_buf(),
            guest_path: ".".to_string(),
            writable: false,
        };
        let small = FilesystemWatchHub::new_with_service(
            service.clone(),
            Some(&grant),
            small_coordination.clone(),
            2,
            1,
        )?;
        let large = FilesystemWatchHub::new_with_service(
            service.clone(),
            Some(&grant),
            large_coordination.clone(),
            2,
            4,
        )?;
        let (small_watch, small_notification) = small.create(Path::new("."), true)?;
        await_setup(&small, &small_coordination, small_watch, small_notification).await?;
        let (large_watch, large_notification) = large.create(Path::new("."), true)?;
        await_setup(&large, &large_coordination, large_watch, large_notification).await?;

        let event = Event::new(EventKind::Create(CreateKind::File))
            .add_path(root.path().join("one"))
            .add_path(root.path().join("two"));
        service.emit_for_test(root.path(), RecursiveMode::Recursive, event);
        let (_, small_batch) = small.read_typed(small_watch)?;
        ensure!(
            small_batch
                == vec![super::FilesystemWatchBatchEvent {
                    kind: super::FilesystemWatchEventKind::Rescan,
                    path: ".".to_string(),
                }]
        );
        let large_bytes = large.pending_bytes(large_watch)?;
        let (_, large_batch) = large.read(large_watch, large_bytes)?;
        let large_batch = String::from_utf8(large_batch)?;
        ensure!(large_batch.contains("created\tone") && large_batch.contains("created\ttwo"));

        small.stop();
        large.stop();
        small_coordination.stop();
        large_coordination.stop();
        Ok(())
    }

    #[tokio::test]
    async fn generation_teardown_removes_only_its_shared_subscription() -> Result<()> {
        let root = tempfile::tempdir()?;
        let service = NativeFilesystemWatcherService::new();
        let grant = FilesystemGrant {
            host_path: root.path().to_path_buf(),
            guest_path: ".".to_string(),
            writable: false,
        };
        let (old_coordination, _old_events) =
            CoordinationHub::new(31, CoordinationConfig::default())?;
        let (new_coordination, _new_events) =
            CoordinationHub::new(32, CoordinationConfig::default())?;
        let old = FilesystemWatchHub::new_with_service(
            service.clone(),
            Some(&grant),
            old_coordination.clone(),
            1,
            8,
        )?;
        let new = FilesystemWatchHub::new_with_service(
            service.clone(),
            Some(&grant),
            new_coordination.clone(),
            1,
            8,
        )?;
        let (old_watch, old_notification) = old.create(Path::new("."), true)?;
        await_setup(&old, &old_coordination, old_watch, old_notification).await?;
        let (new_watch, new_notification) = new.create(Path::new("."), true)?;
        await_setup(&new, &new_coordination, new_watch, new_notification).await?;

        old.stop();
        ensure!(service.stats().registrations == 1 && service.stats().subscriptions == 1);
        service.emit_for_test(
            root.path(),
            RecursiveMode::Recursive,
            Event::new(EventKind::Create(CreateKind::File)).add_path(root.path().join("live")),
        );
        ensure!(new.pending_bytes(new_watch)? > 0);
        ensure!(matches!(
            old.pending_bytes(old_watch),
            Err(FilesystemWatchError::Stopped)
        ));
        new.stop();
        ensure!(service.stats().registrations == 0 && service.stats().subscriptions == 0);
        old_coordination.stop();
        new_coordination.stop();
        Ok(())
    }

    #[tokio::test]
    async fn shared_backend_does_not_leak_paths_across_capabilities() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let shared = parent.path().join("shared");
        fs::create_dir(&shared)?;
        let service = NativeFilesystemWatcherService::new();
        let (wide_coordination, _wide_events) =
            CoordinationHub::new(41, CoordinationConfig::default())?;
        let (narrow_coordination, _narrow_events) =
            CoordinationHub::new(42, CoordinationConfig::default())?;
        let wide = FilesystemWatchHub::new_with_service(
            service.clone(),
            Some(&FilesystemGrant {
                host_path: parent.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: false,
            }),
            wide_coordination.clone(),
            1,
            8,
        )?;
        let narrow = FilesystemWatchHub::new_with_service(
            service.clone(),
            Some(&FilesystemGrant {
                host_path: shared.clone(),
                guest_path: ".".to_string(),
                writable: false,
            }),
            narrow_coordination.clone(),
            1,
            8,
        )?;
        let (wide_watch, wide_notification) = wide.create(Path::new("shared"), true)?;
        await_setup(&wide, &wide_coordination, wide_watch, wide_notification).await?;
        let (narrow_watch, narrow_notification) = narrow.create(Path::new("."), true)?;
        await_setup(
            &narrow,
            &narrow_coordination,
            narrow_watch,
            narrow_notification,
        )
        .await?;
        ensure!(service.stats().registrations == 1);

        service.emit_for_test(
            &shared,
            RecursiveMode::Recursive,
            Event::new(EventKind::Create(CreateKind::File))
                .add_path(parent.path().join("outside-secret")),
        );
        ensure!(wide.pending_bytes(wide_watch)? == 0);
        ensure!(narrow.pending_bytes(narrow_watch)? == 0);
        service.emit_for_test(
            &shared,
            RecursiveMode::Recursive,
            Event::new(EventKind::Create(CreateKind::File)).add_path(shared.join("inside")),
        );
        let (_, wide_batch) = wide.read(wide_watch, wide.pending_bytes(wide_watch)?)?;
        let (_, narrow_batch) = narrow.read(narrow_watch, narrow.pending_bytes(narrow_watch)?)?;
        ensure!(String::from_utf8(wide_batch)?.contains("created\tshared/inside"));
        ensure!(String::from_utf8(narrow_batch)?.contains("created\tinside"));
        wide.stop();
        narrow.stop();
        wide_coordination.stop();
        narrow_coordination.stop();
        Ok(())
    }

    #[test]
    fn stale_setup_completion_cannot_attach_to_recreated_registration() -> Result<()> {
        let root = tempfile::tempdir()?;
        let service = NativeFilesystemWatcherService::new();
        let (coordination, _events) = CoordinationHub::new(51, CoordinationConfig::default())?;
        let notification = coordination.create_notification()?;
        let queue = Arc::new(Mutex::new(super::WatchQueue::default()));
        let subscription = Arc::new(super::NativeSubscription {
            alive: AtomicBool::new(true),
            watched_path: root.path().to_path_buf(),
            mode: RecursiveMode::Recursive,
            projection_root: root.path().to_path_buf(),
            coordination: coordination.clone(),
            notification,
            queue: Arc::clone(&queue),
            max_events: 8,
        });
        let key = super::NativeWatchKey {
            path: root.path().to_path_buf(),
            mode: RecursiveMode::Recursive,
        };
        service
            .inner
            .registrations
            .lock()
            .map_err(|_poisoned| anyhow!("watch registry poisoned"))?
            .insert(
                key.clone(),
                super::NativeRegistration {
                    watcher: None,
                    setup_complete: false,
                    subscribers: HashMap::from([(1, Arc::downgrade(&subscription))]),
                    setup_id: 200,
                    _drop_probe: None,
                },
            );
        let stale_watcher = notify::recommended_watcher(|_result| {})?;

        super::install_native_watch_result(&service.inner, &key, 100, stale_watcher, true);

        let registrations = service
            .inner
            .registrations
            .lock()
            .map_err(|_poisoned| anyhow!("watch registry poisoned"))?;
        let current = registrations
            .get(&key)
            .context("replacement registration")?;
        ensure!(current.setup_id == 200);
        ensure!(!current.setup_complete);
        ensure!(current.watcher.is_none());
        ensure!(
            queue
                .lock()
                .map_err(|_poisoned| anyhow!("watch queue poisoned"))?
                .events
                .is_empty()
        );
        drop(registrations);
        service.remove_registration(&key);
        coordination.stop();
        Ok(())
    }

    #[test]
    fn last_unsubscribe_drops_registration_after_registry_unlock() -> Result<()> {
        let root = tempfile::tempdir()?;
        let service = NativeFilesystemWatcherService::new();
        let key = super::NativeWatchKey {
            path: root.path().to_path_buf(),
            mode: RecursiveMode::NonRecursive,
        };
        let registry_was_unlocked = Arc::new(AtomicBool::new(false));
        service
            .inner
            .registrations
            .lock()
            .map_err(|_poisoned| anyhow!("watch registry poisoned"))?
            .insert(
                key.clone(),
                super::NativeRegistration {
                    watcher: None,
                    setup_complete: true,
                    subscribers: HashMap::new(),
                    setup_id: 1,
                    _drop_probe: Some(super::RegistrationDropProbe {
                        inner: Arc::downgrade(&service.inner),
                        registry_was_unlocked: Arc::clone(&registry_was_unlocked),
                    }),
                },
            );

        service.unsubscribe(&key, 1);

        ensure!(registry_was_unlocked.load(Ordering::Acquire));
        ensure!(
            service
                .inner
                .registrations
                .lock()
                .map_err(|_poisoned| anyhow!("watch registry poisoned"))?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn failed_setup_drops_backend_after_registry_unlock() -> Result<()> {
        let root = tempfile::tempdir()?;
        let service = NativeFilesystemWatcherService::new();
        let key = super::NativeWatchKey {
            path: root.path().to_path_buf(),
            mode: RecursiveMode::NonRecursive,
        };
        service
            .inner
            .registrations
            .lock()
            .map_err(|_poisoned| anyhow!("watch registry poisoned"))?
            .insert(
                key.clone(),
                super::NativeRegistration {
                    watcher: None,
                    setup_complete: false,
                    subscribers: HashMap::new(),
                    setup_id: 77,
                    _drop_probe: None,
                },
            );
        let backend_was_dropped_unlocked = Arc::new(AtomicBool::new(false));
        let callback_drop_probe = super::RegistrationDropProbe {
            inner: Arc::downgrade(&service.inner),
            registry_was_unlocked: Arc::clone(&backend_was_dropped_unlocked),
        };
        let failed_watcher = notify::recommended_watcher(move |_result| {
            let _keep_probe_alive = &callback_drop_probe;
        })?;

        super::install_native_watch_result(&service.inner, &key, 77, failed_watcher, false);

        for _attempt in 0..100 {
            if backend_was_dropped_unlocked.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        ensure!(backend_was_dropped_unlocked.load(Ordering::Acquire));
        let registrations = service
            .inner
            .registrations
            .lock()
            .map_err(|_poisoned| anyhow!("watch registry poisoned"))?;
        let registration = registrations.get(&key).context("failed registration")?;
        ensure!(registration.setup_complete);
        ensure!(registration.watcher.is_none());
        drop(registrations);
        service.remove_registration(&key);
        Ok(())
    }

    #[tokio::test]
    async fn failed_notification_retirement_keeps_logical_watch_live() -> Result<()> {
        let root = tempfile::tempdir()?;
        let service = NativeFilesystemWatcherService::new();
        let (coordination, _events) = CoordinationHub::new(61, CoordinationConfig::default())?;
        let watches = FilesystemWatchHub::new_with_service(
            service.clone(),
            Some(&FilesystemGrant {
                host_path: root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: false,
            }),
            coordination.clone(),
            1,
            8,
        )?;
        let (watch, notification) = watches.create(Path::new("."), true)?;
        await_setup(&watches, &coordination, watch, notification).await?;
        coordination.stop();

        ensure!(matches!(
            watches.drop_watch(watch),
            Err(FilesystemWatchError::Stopped)
        ));
        let entries = watches
            .inner
            .watches
            .lock()
            .map_err(|_poisoned| anyhow!("logical watch registry poisoned"))?;
        let entry = entries
            .get(&watch)
            .context("watch retained after failed retirement")?;
        ensure!(
            entry
                .subscription
                .subscription
                .alive
                .load(Ordering::Acquire)
        );
        drop(entries);
        ensure!(service.stats().registrations == 1);
        ensure!(service.stats().subscriptions == 1);

        watches.stop();
        ensure!(service.stats().registrations == 0);
        Ok(())
    }
}
