//! Generation-scoped accounting for snapshot staging and guest-transfer bytes.
//!
//! Snapshot storage bounds do not cover caller preparation buffers or a
//! contiguous copy made for separate guest memory. This prototype supplies the
//! complementary discipline: reserve exact capacity before allocation and keep
//! the charge attached until the owned allocation has been destroyed.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::snapshot_store::SnapshotLease;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteBudgetLimits {
    pub max_live_bytes: usize,
    pub max_live_allocations: usize,
    pub max_buffer_bytes: usize,
}

impl ByteBudgetLimits {
    #[must_use]
    pub const fn is_usable(self) -> bool {
        self.max_live_bytes > 0 && self.max_live_allocations > 0 && self.max_buffer_bytes > 0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ByteBudgetRetention {
    pub live_bytes: usize,
    pub live_allocations: usize,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ByteBudgetError {
    #[error("byte-budget generation is invalidated")]
    Invalidated,
    #[error("snapshot belongs to generation {snapshot}, not byte-budget generation {budget}")]
    ForeignGeneration { snapshot: u64, budget: u64 },
    #[error("buffer has {actual} bytes, exceeding its {limit}-byte limit")]
    BufferBytes { actual: usize, limit: usize },
    #[error("reservation would retain {actual} bytes, exceeding its {limit}-byte limit")]
    LiveBytes { actual: usize, limit: usize },
    #[error(
        "reservation would retain {actual} allocations, exceeding its {limit}-allocation limit"
    )]
    LiveAllocations { actual: usize, limit: usize },
}

/// Failure while capturing bytes from an independently bounded producer.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum CaptureBoundedError<E> {
    #[error(transparent)]
    Budget(#[from] ByteBudgetError),
    #[error("bounded producer failed: {0}")]
    Producer(E),
    #[error(
        "producer returned length {length} and capacity {capacity}, exceeding its {limit}-byte bound"
    )]
    ProducedBufferTooLarge {
        length: usize,
        capacity: usize,
        limit: usize,
    },
}

#[derive(Debug)]
struct BudgetState {
    retention: ByteBudgetRetention,
}

#[derive(Debug)]
struct BudgetAuthority {
    generation: u64,
    active: AtomicBool,
    owners: AtomicUsize,
    limits: ByteBudgetLimits,
    state: Mutex<BudgetState>,
}

/// Shared staging/transfer budget for one plugin generation.
#[derive(Debug)]
pub struct ByteBudget {
    authority: Arc<BudgetAuthority>,
}

impl ByteBudget {
    #[must_use]
    pub fn new(generation: u64, limits: ByteBudgetLimits) -> Self {
        Self {
            authority: Arc::new(BudgetAuthority {
                generation,
                active: AtomicBool::new(true),
                owners: AtomicUsize::new(1),
                limits,
                state: Mutex::new(BudgetState {
                    retention: ByteBudgetRetention::default(),
                }),
            }),
        }
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.authority.generation
    }

    #[must_use]
    pub fn limits(&self) -> ByteBudgetLimits {
        self.authority.limits
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.authority.active.load(Ordering::Acquire)
    }

    /// Revokes every outstanding buffer without waiting for readers or drops.
    pub fn invalidate(&self) {
        self.authority.active.store(false, Ordering::Release);
    }

    #[must_use]
    pub fn retention(&self) -> ByteBudgetRetention {
        self.lock_state().retention
    }

    fn reserve(&self, bytes: usize) -> Result<Reservation, ByteBudgetError> {
        if bytes > self.authority.limits.max_buffer_bytes {
            return Err(ByteBudgetError::BufferBytes {
                actual: bytes,
                limit: self.authority.limits.max_buffer_bytes,
            });
        }
        if !self.is_active() {
            return Err(ByteBudgetError::Invalidated);
        }
        let mut state = self.lock_state();
        if !self.is_active() {
            return Err(ByteBudgetError::Invalidated);
        }
        let live_bytes =
            state
                .retention
                .live_bytes
                .checked_add(bytes)
                .ok_or(ByteBudgetError::LiveBytes {
                    actual: usize::MAX,
                    limit: self.authority.limits.max_live_bytes,
                })?;
        if live_bytes > self.authority.limits.max_live_bytes {
            return Err(ByteBudgetError::LiveBytes {
                actual: live_bytes,
                limit: self.authority.limits.max_live_bytes,
            });
        }
        let live_allocations = state.retention.live_allocations.checked_add(1).ok_or(
            ByteBudgetError::LiveAllocations {
                actual: usize::MAX,
                limit: self.authority.limits.max_live_allocations,
            },
        )?;
        if live_allocations > self.authority.limits.max_live_allocations {
            return Err(ByteBudgetError::LiveAllocations {
                actual: live_allocations,
                limit: self.authority.limits.max_live_allocations,
            });
        }
        state.retention = ByteBudgetRetention {
            live_bytes,
            live_allocations,
        };
        let reservation = Reservation {
            bytes,
            authority: Arc::clone(&self.authority),
        };
        drop(state);
        Ok(reservation)
    }

    fn lock_state(&self) -> MutexGuard<'_, BudgetState> {
        self.authority
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Clone for ByteBudget {
    fn clone(&self) -> Self {
        self.authority.owners.fetch_add(1, Ordering::Relaxed);
        Self {
            authority: Arc::clone(&self.authority),
        }
    }
}

impl Drop for ByteBudget {
    fn drop(&mut self) {
        if self.authority.owners.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.invalidate();
        }
    }
}

#[derive(Debug)]
struct Reservation {
    bytes: usize,
    authority: Arc<BudgetAuthority>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self
            .authority
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.retention.live_bytes -= self.bytes;
        state.retention.live_allocations -= 1;
    }
}

/// A private owned byte allocation whose reservation cannot be detached.
#[derive(Debug)]
pub struct BudgetedBytes {
    bytes: Vec<u8>,
    reservation: Reservation,
    source: Option<SnapshotLease>,
    copy_time: Duration,
}

impl BudgetedBytes {
    /// Copies caller staging data only after reserving its exact byte count.
    pub fn copy_from_slice(budget: &ByteBudget, source: &[u8]) -> Result<Self, ByteBudgetError> {
        let started = Instant::now();
        let reservation = budget.reserve(source.len())?;
        let bytes = source.to_vec();
        if !budget.is_active() {
            drop(bytes);
            drop(reservation);
            return Err(ByteBudgetError::Invalidated);
        }
        Ok(Self {
            bytes,
            reservation,
            source: None,
            copy_time: started.elapsed(),
        })
    }

    /// Flattens a coherent immutable snapshot after reserving transfer bytes.
    pub fn copy_snapshot(
        budget: &ByteBudget,
        snapshot: SnapshotLease,
    ) -> Result<Self, ByteBudgetError> {
        if snapshot.generation() != budget.generation() {
            return Err(ByteBudgetError::ForeignGeneration {
                snapshot: snapshot.generation(),
                budget: budget.generation(),
            });
        }
        if !snapshot.is_authoritative() {
            return Err(ByteBudgetError::Invalidated);
        }
        let started = Instant::now();
        let reservation = budget.reserve(snapshot.logical_bytes())?;
        let mut bytes = Vec::with_capacity(snapshot.logical_bytes());
        for chunk in snapshot.iter_chunks() {
            bytes.extend_from_slice(chunk);
        }
        if !budget.is_active() || !snapshot.is_authoritative() {
            drop(bytes);
            drop(reservation);
            return Err(ByteBudgetError::Invalidated);
        }
        Ok(Self {
            bytes,
            reservation,
            source: Some(snapshot),
            copy_time: started.elapsed(),
        })
    }

    /// Captures an owned buffer from a trusted producer with an independent
    /// allocation bound, reserving that entire bound before calling it.
    ///
    /// This generic quota cannot constrain allocations performed inside an
    /// arbitrary callback. `producer` must already enforce `max_bytes` before
    /// allocating (for example through a Wasmtime component memory and flat-list
    /// limit). This method supplies shared host admission and verifies the owned
    /// result's length and capacity before making it available. The conservative
    /// `max_bytes` reservation remains charged until the buffer is destroyed.
    pub fn capture_bounded<E>(
        budget: &ByteBudget,
        max_bytes: usize,
        producer: impl FnOnce() -> Result<Vec<u8>, E>,
    ) -> Result<Self, CaptureBoundedError<E>> {
        let started = Instant::now();
        let reservation = budget
            .reserve(max_bytes)
            .map_err(CaptureBoundedError::Budget)?;
        let bytes = producer().map_err(CaptureBoundedError::Producer)?;
        if bytes.len() > max_bytes || bytes.capacity() > max_bytes {
            return Err(CaptureBoundedError::ProducedBufferTooLarge {
                length: bytes.len(),
                capacity: bytes.capacity(),
                limit: max_bytes,
            });
        }
        if !budget.is_active() {
            return Err(CaptureBoundedError::Budget(ByteBudgetError::Invalidated));
        }
        Ok(Self {
            bytes,
            reservation,
            source: None,
            copy_time: started.elapsed(),
        })
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    #[must_use]
    pub const fn copy_time(&self) -> Duration {
        self.copy_time
    }

    #[must_use]
    pub fn is_authoritative(&self) -> bool {
        self.reservation.authority.active.load(Ordering::Acquire)
            && self
                .source
                .as_ref()
                .is_none_or(SnapshotLease::is_authoritative)
    }

    pub fn bytes(&self) -> Result<&[u8], ByteBudgetError> {
        if !self.is_authoritative() {
            return Err(ByteBudgetError::Invalidated);
        }
        Ok(&self.bytes)
    }
}

impl Drop for BudgetedBytes {
    fn drop(&mut self) {
        let bytes = std::mem::take(&mut self.bytes);
        // Field drop releases reservation only after this explicit payload free.
        drop(bytes);
    }
}

/// An aggregate decoded value whose producer enforces a known allocation
/// ceiling.
///
/// The reservation covers all nested allocations as one bounded group;
/// the decoder must separately bound their count. No owned value can escape.
#[derive(Debug)]
pub struct BudgetedValue<T> {
    value: Option<T>,
    reservation: Reservation,
}

impl<T> BudgetedValue<T> {
    pub fn capture<E>(
        budget: &ByteBudget,
        max_bytes: usize,
        produce: impl FnOnce() -> Result<T, E>,
    ) -> Result<Self, CaptureBoundedError<E>> {
        let reservation = budget.reserve(max_bytes)?;
        let value = produce().map_err(CaptureBoundedError::Producer)?;
        if !budget.is_active() {
            return Err(CaptureBoundedError::Budget(ByteBudgetError::Invalidated));
        }
        Ok(Self {
            value: Some(value),
            reservation,
        })
    }

    pub fn value(&self) -> Result<&T, ByteBudgetError> {
        if !self.reservation.authority.active.load(Ordering::Acquire) {
            return Err(ByteBudgetError::Invalidated);
        }
        self.value.as_ref().ok_or(ByteBudgetError::Invalidated)
    }
}

impl<T> Drop for BudgetedValue<T> {
    fn drop(&mut self) {
        // Destroy the complete allocation graph before releasing its credit.
        drop(self.value.take());
    }
}
