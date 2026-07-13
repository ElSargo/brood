//! Capability-rooted host filesystem watches shared by one actor generation.

use std::{
    collections::{HashMap, VecDeque},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
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

struct FilesystemWatchInner {
    root: Option<PathBuf>,
    coordination: CoordinationHub,
    next_watch: AtomicU64,
    alive: AtomicBool,
    max_watches: usize,
    max_events: usize,
    watches: Mutex<HashMap<FilesystemWatchId, FilesystemWatchEntry>>,
    setup_threads: Mutex<HashMap<FilesystemWatchId, JoinHandle<()>>>,
}

struct FilesystemWatchEntry {
    // Recursive backend registration may walk a large tree. The entry is
    // reachable while that host-owned setup is pending, then owns the live
    // watcher after setup completes.
    watcher: Option<RecommendedWatcher>,
    setup_complete: bool,
    notification: NotificationId,
    queue: Arc<Mutex<WatchQueue>>,
}

#[derive(Default)]
struct WatchQueue {
    generation: u64,
    events: VecDeque<WatchEvent>,
}

struct WatchEvent {
    generation: u64,
    line: String,
}

impl FilesystemWatchHub {
    pub(super) fn new(
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
                watches: Mutex::new(HashMap::new()),
                setup_threads: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub(super) fn create(
        &self,
        guest_path: &Path,
        recursive: bool,
    ) -> Result<(FilesystemWatchId, NotificationId), FilesystemWatchError> {
        self.reap_finished_setup_threads()?;
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
        let mut setup_threads = self
            .inner
            .setup_threads
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)?;
        let orphaned_setups = setup_threads
            .keys()
            .filter(|watch| !watches.contains_key(watch))
            .count();
        if watches.len().saturating_add(orphaned_setups) >= self.inner.max_watches {
            return Err(FilesystemWatchError::Limit);
        }
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
        let callback_queue = Arc::clone(&queue);
        let callback_root = root.clone();
        let callback_coordination = self.inner.coordination.clone();
        let max_events = self.inner.max_events;
        let watcher = notify::recommended_watcher(move |result| {
            record_callback(
                &callback_queue,
                &callback_coordination,
                notification,
                &callback_root,
                max_events,
                result,
            );
        });
        let watcher = match watcher {
            Ok(watcher) => watcher,
            Err(error) => {
                let _drop_result = self.inner.coordination.retire_notification(notification);
                return Err(FilesystemWatchError::Backend(error.to_string()));
            }
        };
        watches.insert(
            id,
            FilesystemWatchEntry {
                watcher: None,
                setup_complete: false,
                notification,
                queue,
            },
        );
        drop(watches);

        // `notify` recursively registers every existing directory before
        // returning. Doing that in this host call would block the actor which
        // requested the watch and can exceed its dispatcher deadline for a
        // real build tree. Registration is therefore host-owned background
        // work. Its first notification is always a rescan marker because
        // changes can race the registration walk.
        if !self.inner.alive.load(Ordering::Acquire) {
            let _removed = self
                .inner
                .watches
                .lock()
                .map(|mut watches| watches.remove(&id));
            let _drop_result = self.inner.coordination.retire_notification(notification);
            return Err(FilesystemWatchError::Stopped);
        }
        let inner = Arc::clone(&self.inner);
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let setup = thread::Builder::new()
            .name("manifold-fs-watch-setup".to_string())
            .spawn(move || finish_watch_setup(inner, id, watcher, watched_path, mode))
            .map_err(|error| {
                if let Ok(mut watches) = self.inner.watches.lock() {
                    watches.remove(&id);
                }
                let _drop_result = self.inner.coordination.retire_notification(notification);
                FilesystemWatchError::Backend(error.to_string())
            })?;
        setup_threads.insert(id, setup);
        drop(setup_threads);
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
            .map(|queue| queue.events.iter().map(|event| event.line.len()).sum())
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
            if bytes.len().saturating_add(event.line.len()) > capacity {
                break;
            }
            let Some(event) = queue.events.pop_front() else {
                break;
            };
            generation = event.generation;
            bytes.extend_from_slice(event.line.as_bytes());
        }
        if bytes.is_empty() {
            return Err(FilesystemWatchError::Limit);
        }
        drop(queue);
        Ok((generation, bytes))
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
        let setup_complete = entry.setup_complete;
        let entry = watches
            .remove(&watch)
            .ok_or(FilesystemWatchError::Invalid)?;
        drop(watches);
        drop(entry.watcher);

        if setup_complete {
            let setup = self
                .inner
                .setup_threads
                .lock()
                .map_err(|_poisoned| FilesystemWatchError::Unavailable)?
                .remove(&watch);
            if let Some(setup) = setup {
                let _join_result = setup.join();
            }
        } else {
            self.reap_finished_setup_threads()?;
        }
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
                drop(entry.watcher);
                let _drop_result = self
                    .inner
                    .coordination
                    .retire_notification(entry.notification);
            }
        }
        if let Ok(mut setup_threads) = self.inner.setup_threads.lock() {
            let threads = setup_threads.drain().collect::<Vec<_>>();
            drop(setup_threads);
            for (_watch, thread) in threads {
                let _join_result = thread.join();
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

    fn reap_finished_setup_threads(&self) -> Result<(), FilesystemWatchError> {
        let mut setup_threads = self
            .inner
            .setup_threads
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)?;
        let finished = setup_threads
            .extract_if(|_watch, thread| thread.is_finished())
            .map(|(_watch, thread)| thread)
            .collect::<Vec<_>>();
        drop(setup_threads);
        for thread in finished {
            let _join_result = thread.join();
        }
        Ok(())
    }

    #[cfg(test)]
    fn setup_thread_count(&self) -> Result<usize, FilesystemWatchError> {
        self.reap_finished_setup_threads()?;
        self.inner
            .setup_threads
            .lock()
            .map_err(|_poisoned| FilesystemWatchError::Unavailable)
            .map(|threads| threads.len())
    }
}

fn finish_watch_setup(
    inner: Arc<FilesystemWatchInner>,
    watch: FilesystemWatchId,
    mut watcher: RecommendedWatcher,
    watched_path: PathBuf,
    mode: RecursiveMode,
) {
    let result = watcher.watch(&watched_path, mode);
    let Ok(mut watches) = inner.watches.lock() else {
        return;
    };
    let Some(entry) = watches.get_mut(&watch) else {
        return;
    };
    if !inner.alive.load(Ordering::Acquire) {
        return;
    }
    if result.is_ok() {
        entry.watcher = Some(watcher);
    }
    entry.setup_complete = true;
    let queue = Arc::clone(&entry.queue);
    let notification = entry.notification;
    drop(watches);

    // Whether registration succeeded or failed, consumers must reconcile the
    // interval during which the recursive tree was being installed. On a
    // backend failure this also makes the otherwise inert handle observable.
    let Ok(mut queue) = queue.lock() else {
        return;
    };
    queue.generation = queue.generation.saturating_add(1).max(1);
    let generation = queue.generation;
    push_overflow(&mut queue.events, generation);
    drop(queue);
    let _signal_result = inner.coordination.signal(notification, generation);
}

fn record_callback(
    queue: &Mutex<WatchQueue>,
    coordination: &CoordinationHub,
    notification: NotificationId,
    root: &Path,
    max_events: usize,
    result: notify::Result<Event>,
) {
    let Ok(mut queue) = queue.lock() else {
        return;
    };
    match result {
        Ok(event) => {
            let Some(label) = event_label(event.kind) else {
                return;
            };
            queue.generation = queue.generation.saturating_add(1).max(1);
            let generation = queue.generation;
            for path in event.paths {
                let Ok(path) = path.strip_prefix(root) else {
                    continue;
                };
                push_bounded_event(&mut queue.events, max_events, generation, label, path);
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
    let _signal_result = coordination.signal(notification, generation);
}

fn push_bounded_event(
    events: &mut VecDeque<WatchEvent>,
    max_events: usize,
    generation: u64,
    label: &str,
    path: &Path,
) {
    if events.len() >= max_events {
        push_overflow(events, generation);
        return;
    }
    let path = path.to_string_lossy();
    events.push_back(WatchEvent {
        generation,
        line: format!("{label}\t{path}\n"),
    });
}

fn push_overflow(events: &mut VecDeque<WatchEvent>, generation: u64) {
    events.clear();
    events.push_back(WatchEvent {
        generation,
        line: "rescan\t.\n".to_string(),
    });
}

const fn event_label(kind: EventKind) -> Option<&'static str> {
    match kind {
        EventKind::Create(_) => Some("created"),
        EventKind::Modify(_) => Some("modified"),
        EventKind::Remove(_) => Some("removed"),
        EventKind::Access(_) => None,
        EventKind::Other | EventKind::Any => Some("changed"),
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
    use std::{fs, path::Path, time::Duration};

    use anyhow::{Context, Result, ensure};

    use super::{FilesystemWatchError, FilesystemWatchHub};
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
            ensure!(watches.setup_thread_count()? == 0);
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
}
