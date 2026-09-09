//! Bounded background ownership for snapshot generation retirement.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;

use crate::snapshot_store::{
    PreparedChunkedSnapshot, PreparedCopiedSnapshot, PublicationMeasurement, SnapshotLease,
    SnapshotRetention, SnapshotStore, SnapshotStoreError, SnapshotStoreLimits,
};

pub const MAX_MANAGED_SNAPSHOT_GENERATIONS: usize = 64;
pub const MAX_MANAGED_SNAPSHOT_METADATA_ENTRIES: usize = 16_384;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotReclaimerConfig {
    pub max_generations: usize,
    pub poll_interval: Duration,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SnapshotReclaimerError {
    #[error("snapshot reclaimer requires non-zero generation capacity and poll interval")]
    InvalidConfig,
    #[error("managed snapshot-store limits are unusable or exceed metadata ceilings")]
    InvalidStoreLimits,
    #[error("snapshot reclaimer has reached its {limit}-generation limit")]
    GenerationCapacity { limit: usize },
    #[error("snapshot reclaimer stopped")]
    Stopped,
}

#[derive(Debug)]
struct Registration {
    store: Arc<SnapshotStore>,
    retiring: Arc<AtomicBool>,
}

#[derive(Debug)]
struct Entry {
    store: Arc<SnapshotStore>,
    retiring: Arc<AtomicBool>,
}

/// One bounded worker owns registered stores through final off-thread release.
#[derive(Debug)]
pub struct BackgroundSnapshotReclaimer {
    sender: Option<SyncSender<Registration>>,
    active_generations: Arc<AtomicUsize>,
    max_generations: usize,
    thread: Option<JoinHandle<()>>,
}

impl BackgroundSnapshotReclaimer {
    pub fn new(config: SnapshotReclaimerConfig) -> Result<Self, SnapshotReclaimerError> {
        if config.max_generations == 0
            || config.max_generations > MAX_MANAGED_SNAPSHOT_GENERATIONS
            || config.poll_interval.is_zero()
        {
            return Err(SnapshotReclaimerError::InvalidConfig);
        }
        let (sender, receiver) = mpsc::sync_channel(config.max_generations);
        let active_generations = Arc::new(AtomicUsize::new(0));
        let worker_active = Arc::clone(&active_generations);
        let thread = thread::Builder::new()
            .name("brood-snapshot-reclaimer".to_owned())
            .spawn(move || run_reclaimer(receiver, config, worker_active))
            .map_err(|_spawn_error| SnapshotReclaimerError::Stopped)?;
        Ok(Self {
            sender: Some(sender),
            active_generations,
            max_generations: config.max_generations,
            thread: Some(thread),
        })
    }

    /// Registers a newly owned generation before returning it to producers.
    pub fn register(
        &self,
        generation: u64,
        limits: SnapshotStoreLimits,
    ) -> Result<ManagedSnapshotStore, SnapshotReclaimerError> {
        let sender = self
            .sender
            .as_ref()
            .ok_or(SnapshotReclaimerError::Stopped)?;
        if !limits.is_usable()
            || limits.max_live_allocations > MAX_MANAGED_SNAPSHOT_METADATA_ENTRIES
            || limits.max_live_roots > MAX_MANAGED_SNAPSHOT_METADATA_ENTRIES
            || limits.max_chunks_per_snapshot > MAX_MANAGED_SNAPSHOT_METADATA_ENTRIES
        {
            return Err(SnapshotReclaimerError::InvalidStoreLimits);
        }
        self.reserve_generation()?;
        let store = Arc::new(SnapshotStore::new(generation, limits));
        let retiring = Arc::new(AtomicBool::new(false));
        match sender.try_send(Registration {
            store: Arc::clone(&store),
            retiring: Arc::clone(&retiring),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.active_generations.fetch_sub(1, Ordering::AcqRel);
                return Err(SnapshotReclaimerError::Stopped);
            }
        }
        Ok(ManagedSnapshotStore {
            store,
            retiring,
            _keepalive: sender.clone(),
        })
    }

    #[must_use]
    pub fn active_generations(&self) -> usize {
        self.active_generations.load(Ordering::Acquire)
    }

    fn reserve_generation(&self) -> Result<(), SnapshotReclaimerError> {
        let mut current = self.active_generations.load(Ordering::Acquire);
        loop {
            if current == self.max_generations {
                return Err(SnapshotReclaimerError::GenerationCapacity {
                    limit: self.max_generations,
                });
            }
            match self.active_generations.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for BackgroundSnapshotReclaimer {
    fn drop(&mut self) {
        // Disconnect without joining. The worker retains all generations and
        // exits only after managed owners and their external pins have drained.
        drop(self.sender.take());
        drop(self.thread.take());
    }
}

/// Producer-facing generation handle. Dropping it only revokes and marks the
/// generation; accepted storage remains owned by the background worker.
#[derive(Debug)]
pub struct ManagedSnapshotStore {
    store: Arc<SnapshotStore>,
    retiring: Arc<AtomicBool>,
    _keepalive: SyncSender<Registration>,
}

impl ManagedSnapshotStore {
    pub(crate) fn invalidate(&self) {
        self.store.invalidate();
    }
    pub fn publish_copied(
        &self,
        prepared: PreparedCopiedSnapshot,
    ) -> Result<PublicationMeasurement, SnapshotStoreError> {
        self.store.publish_copied(prepared)
    }

    pub fn publish_copy_of(
        &self,
        revision: u64,
        bytes: &[u8],
    ) -> Result<PublicationMeasurement, SnapshotStoreError> {
        self.store.publish_copy_of(revision, bytes)
    }

    pub fn publish_chunks(
        &self,
        prepared: PreparedChunkedSnapshot,
    ) -> Result<PublicationMeasurement, SnapshotStoreError> {
        self.store.publish_chunks(prepared)
    }

    pub fn acquire_latest(&self) -> Result<Option<SnapshotLease>, SnapshotStoreError> {
        self.store.acquire_latest()
    }

    #[must_use]
    pub fn retention(&self) -> SnapshotRetention {
        self.store.retention()
    }
}

impl Drop for ManagedSnapshotStore {
    fn drop(&mut self) {
        self.store.invalidate();
        self.retiring.store(true, Ordering::Release);
    }
}

fn run_reclaimer(
    receiver: Receiver<Registration>,
    config: SnapshotReclaimerConfig,
    active_generations: Arc<AtomicUsize>,
) {
    let mut entries = Vec::with_capacity(config.max_generations);
    let mut disconnected = false;
    loop {
        let registration = if entries.is_empty() {
            match receiver.recv() {
                Ok(registration) => Some(registration),
                Err(_disconnected) => break,
            }
        } else {
            match receiver.recv_timeout(config.poll_interval) {
                Ok(registration) => Some(registration),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    None
                }
            }
        };
        if let Some(registration) = registration {
            entries.push(Entry {
                store: registration.store,
                retiring: registration.retiring,
            });
        }

        let mut index = 0;
        while index < entries.len() {
            let Some(entry) = entries.get(index) else {
                break;
            };
            entry.store.reclaim_retired();
            if entry.retiring.load(Ordering::Acquire)
                && Arc::strong_count(&entry.store) == 1
                && entry.store.retention()
                    == (SnapshotRetention {
                        live_bytes: 0,
                        live_allocations: 0,
                        live_roots: 0,
                    })
            {
                entries.swap_remove(index);
                active_generations.fetch_sub(1, Ordering::AcqRel);
            } else {
                index += 1;
            }
        }
        if disconnected && entries.is_empty() {
            break;
        }
        if disconnected {
            thread::sleep(config.poll_interval);
        }
    }
}
