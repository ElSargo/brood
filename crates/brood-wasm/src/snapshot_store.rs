//! Bounded immutable snapshot storage prototype for pure plugin facets.
//!
//! This module intentionally describes a host-side storage experiment, not a
//! guest ABI. It keeps allocation accounting attached to the allocation rather
//! than a root, so superseded roots retained by render or input leases remain
//! visible to admission control.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use thiserror::Error;

/// Bounds applied to one plugin generation's published application views.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotStoreLimits {
    /// Payload bytes owned by accepted/assembled roots and exported chunks.
    /// Caller preparation and guest-transfer buffers are measured separately.
    pub max_live_bytes: usize,
    pub max_live_allocations: usize,
    pub max_live_roots: usize,
    pub max_snapshot_bytes: usize,
    pub max_chunks_per_snapshot: usize,
}

impl SnapshotStoreLimits {
    /// Returns whether every limit can admit some non-empty useful state.
    #[must_use]
    pub const fn is_usable(self) -> bool {
        self.max_live_bytes > 0
            && self.max_live_allocations > 0
            && self.max_live_roots > 0
            && self.max_snapshot_bytes > 0
            && self.max_chunks_per_snapshot > 0
    }
}

/// Storage strategy being measured by the prototype.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotStorageKind {
    Copied,
    ImmutableChunks,
}

/// Unpublished copied view. Its construction cost is deliberately separate
/// from the short publication critical section.
#[derive(Debug)]
pub struct PreparedCopiedSnapshot {
    revision: u64,
    bytes: Vec<u8>,
    preparation_time: Duration,
}

impl PreparedCopiedSnapshot {
    #[must_use]
    pub const fn new(revision: u64, bytes: Vec<u8>) -> Self {
        Self {
            revision,
            bytes,
            preparation_time: Duration::ZERO,
        }
    }

    /// Builds a coherent view while measuring construction independently.
    pub fn build(revision: u64, build: impl FnOnce() -> Vec<u8>) -> Self {
        let started = Instant::now();
        let bytes = build();
        Self {
            revision,
            bytes,
            preparation_time: started.elapsed(),
        }
    }
}

/// One chunk in a prepared chunked revision.
#[derive(Clone, Debug)]
pub enum PreparedChunk {
    New(Vec<u8>),
    Shared(ImmutableChunk),
}

/// Unpublished immutable-chunk view.
#[derive(Debug)]
pub struct PreparedChunkedSnapshot {
    revision: u64,
    chunks: Vec<PreparedChunk>,
    preparation_time: Duration,
}

impl PreparedChunkedSnapshot {
    #[must_use]
    pub const fn new(revision: u64, chunks: Vec<PreparedChunk>) -> Self {
        Self {
            revision,
            chunks,
            preparation_time: Duration::ZERO,
        }
    }

    pub fn build(revision: u64, build: impl FnOnce() -> Vec<PreparedChunk>) -> Self {
        let started = Instant::now();
        let chunks = build();
        Self {
            revision,
            chunks,
            preparation_time: started.elapsed(),
        }
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SnapshotStoreError {
    #[error("snapshot store generation is invalidated")]
    Invalidated,
    #[error("revision {attempted} does not advance latest revision {latest}")]
    StaleRevision { attempted: u64, latest: u64 },
    #[error("snapshot has {actual} bytes, exceeding its {limit}-byte limit")]
    SnapshotBytes { actual: usize, limit: usize },
    #[error("snapshot has {actual} chunks, exceeding its {limit}-chunk limit")]
    SnapshotChunks { actual: usize, limit: usize },
    #[error("publication would retain {actual} live bytes, exceeding its {limit}-byte limit")]
    LiveBytes { actual: usize, limit: usize },
    #[error(
        "publication would retain {actual} allocations, exceeding its {limit}-allocation limit"
    )]
    LiveAllocations { actual: usize, limit: usize },
    #[error("publication would retain {actual} roots, exceeding its {limit}-root limit")]
    LiveRoots { actual: usize, limit: usize },
    #[error("shared chunk belongs to a different snapshot store generation")]
    ForeignChunk,
    #[error("snapshot size overflow")]
    SizeOverflow,
}

/// Measurements for one successful root exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicationMeasurement {
    pub revision: u64,
    pub kind: SnapshotStorageKind,
    pub logical_bytes: usize,
    pub newly_allocated_bytes: usize,
    pub newly_allocated_chunks: usize,
    pub preparation_time: Duration,
    /// Admission validation and immutable root assembly before root exchange.
    pub assembly_time: Duration,
    /// Root-lock acquisition and final coherent exchange, including lock release.
    pub root_exchange_time: Duration,
    /// End-to-end publish call latency, excluding caller-side preparation.
    pub publication_time: Duration,
    pub live_bytes_after_publication: usize,
    pub live_allocations_after_publication: usize,
    pub live_roots_after_publication: usize,
}

/// Current payload retention, including superseded leased roots.
///
/// Metadata is bounded independently by allocation/root/chunk limits. These
/// counters do not represent allocator overhead or caller-owned transfer data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotRetention {
    /// Payload bytes. Allocation and root metadata have separate count bounds.
    pub live_bytes: usize,
    pub live_allocations: usize,
    pub live_roots: usize,
}

#[derive(Debug, Default)]
struct Accounting {
    bytes: AtomicUsize,
    allocations: AtomicUsize,
    roots: AtomicUsize,
}

#[derive(Debug)]
struct GenerationAuthority {
    id: u64,
    active: AtomicBool,
    accounting: Accounting,
}

#[derive(Debug)]
struct Allocation {
    bytes: Box<[u8]>,
    authority: Arc<GenerationAuthority>,
}

impl Drop for Allocation {
    fn drop(&mut self) {
        let bytes = std::mem::take(&mut self.bytes);
        let length = bytes.len();
        // Do not release admission credit while the payload is still live.
        drop(bytes);
        self.authority
            .accounting
            .bytes
            .fetch_sub(length, Ordering::AcqRel);
        self.authority
            .accounting
            .allocations
            .fetch_sub(1, Ordering::AcqRel);
    }
}

/// Reusable immutable allocation from a published chunked view.
#[derive(Clone, Debug)]
pub struct ImmutableChunk {
    allocation: Arc<Allocation>,
}

impl ImmutableChunk {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.allocation.bytes
    }
}

#[derive(Debug)]
enum SnapshotPayload {
    Copied(Arc<Allocation>),
    // Exact-length storage prevents retaining excess capacity supplied in a
    // prepared descriptor Vec (including in-place collection's allocation).
    Chunks(Box<[Arc<Allocation>]>),
}

#[derive(Debug)]
struct SnapshotRoot {
    revision: u64,
    logical_bytes: usize,
    payload: SnapshotPayload,
    authority: Arc<GenerationAuthority>,
}

impl Drop for SnapshotRoot {
    fn drop(&mut self) {
        let payload = std::mem::replace(&mut self.payload, SnapshotPayload::Chunks(Box::default()));
        drop(payload);
        self.authority
            .accounting
            .roots
            .fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct StoreState {
    latest: Option<Arc<SnapshotRoot>>,
    retired: Vec<Arc<SnapshotRoot>>,
}

#[derive(Debug)]
struct RetirementState {
    spare: Vec<Arc<SnapshotRoot>>,
    pinned: Vec<Arc<SnapshotRoot>>,
}

#[derive(Debug)]
struct AllocationState {
    accepted: Vec<Arc<Allocation>>,
}

#[derive(Debug)]
struct AllocationRetirementState {
    spare: Vec<Arc<Allocation>>,
    pinned: Vec<Arc<Allocation>>,
}

/// Host-owned, generation-scoped immutable root store.
///
/// The prototype's owner must drive reclamation on a background lane and keep
/// the store alive until pins drain. Dropping the store revokes authority, but
/// is not itself an asynchronous shutdown protocol: remaining pins can then
/// perform final destruction on their dropping thread.
#[derive(Debug)]
pub struct SnapshotStore {
    limits: SnapshotStoreLimits,
    authority: Arc<GenerationAuthority>,
    publish_gate: Mutex<()>,
    state: Mutex<StoreState>,
    retirement: Mutex<RetirementState>,
    allocations: Mutex<AllocationState>,
    allocation_retirement: Mutex<AllocationRetirementState>,
}

impl Drop for SnapshotStore {
    fn drop(&mut self) {
        // Revocation precedes field destruction, so a lease outliving the store
        // cannot retain generation authority even though it retains memory.
        self.authority.active.store(false, Ordering::Release);
    }
}

impl SnapshotStore {
    #[must_use]
    pub fn new(generation: u64, limits: SnapshotStoreLimits) -> Self {
        Self {
            limits,
            authority: Arc::new(GenerationAuthority {
                id: generation,
                active: AtomicBool::new(true),
                accounting: Accounting::default(),
            }),
            publish_gate: Mutex::new(()),
            state: Mutex::new(StoreState {
                latest: None,
                retired: Vec::with_capacity(limits.max_live_roots),
            }),
            retirement: Mutex::new(RetirementState {
                spare: Vec::with_capacity(limits.max_live_roots),
                pinned: Vec::with_capacity(limits.max_live_roots),
            }),
            allocations: Mutex::new(AllocationState {
                accepted: Vec::with_capacity(limits.max_live_allocations),
            }),
            allocation_retirement: Mutex::new(AllocationRetirementState {
                spare: Vec::with_capacity(limits.max_live_allocations),
                pinned: Vec::with_capacity(limits.max_live_allocations),
            }),
        }
    }

    pub fn publish_copied(
        &self,
        prepared: PreparedCopiedSnapshot,
    ) -> Result<PublicationMeasurement, SnapshotStoreError> {
        self.publish_copy_with(
            prepared.revision,
            prepared.bytes.len(),
            prepared.preparation_time,
            || prepared.bytes,
        )
    }

    /// Copies borrowed staging bytes only after admitting the destination root.
    ///
    /// The caller keeps its staging allocation charged while this call runs.
    /// Publisher serialization holds admission stable until the destination is
    /// charged; reclamation can only reduce existing retention in that interval.
    pub fn publish_copy_of(
        &self,
        revision: u64,
        bytes: &[u8],
    ) -> Result<PublicationMeasurement, SnapshotStoreError> {
        self.publish_copy_with(revision, bytes.len(), Duration::ZERO, || bytes.to_vec())
    }

    fn publish_copy_with(
        &self,
        revision: u64,
        logical_bytes: usize,
        preparation_time: Duration,
        prepare: impl FnOnce() -> Vec<u8>,
    ) -> Result<PublicationMeasurement, SnapshotStoreError> {
        let publication_started = Instant::now();
        let _publish = self.lock_publish_gate();
        let state = self.lock_state();
        self.validate_revision(&state, revision)?;
        self.validate_shape(logical_bytes, 1)?;
        self.validate_admission(logical_bytes, 1)?;
        drop(state);

        let allocation = self.allocate_with(logical_bytes, prepare);
        let root = self.make_root(revision, logical_bytes, SnapshotPayload::Copied(allocation));
        let assembly_time = publication_started.elapsed();
        let exchange_started = Instant::now();
        let mut state = self.lock_state();
        if !self.authority.active.load(Ordering::Acquire) {
            state.retired.push(root);
            return Err(SnapshotStoreError::Invalidated);
        }
        if let Some(previous) = state.latest.replace(root) {
            state.retired.push(previous);
        }
        drop(state);
        let root_exchange_time = exchange_started.elapsed();
        Ok(self.measurement(
            revision,
            SnapshotStorageKind::Copied,
            logical_bytes,
            logical_bytes,
            1,
            preparation_time,
            assembly_time,
            root_exchange_time,
            publication_started.elapsed(),
        ))
    }

    pub fn publish_chunks(
        &self,
        prepared: PreparedChunkedSnapshot,
    ) -> Result<PublicationMeasurement, SnapshotStoreError> {
        let publication_started = Instant::now();
        let _publish = self.lock_publish_gate();
        let state = self.lock_state();
        self.validate_revision(&state, prepared.revision)?;
        drop(state);

        // Reject an oversized descriptor list before walking any guest-shaped
        // entries; otherwise admission itself could exceed the chunk budget.
        self.validate_shape(0, prepared.chunks.len())?;
        let mut logical_bytes = 0usize;
        let mut new_bytes = 0usize;
        let mut new_allocations = 0usize;
        for chunk in &prepared.chunks {
            let length = match chunk {
                PreparedChunk::New(bytes) => {
                    new_bytes = new_bytes
                        .checked_add(bytes.len())
                        .ok_or(SnapshotStoreError::SizeOverflow)?;
                    new_allocations = new_allocations
                        .checked_add(1)
                        .ok_or(SnapshotStoreError::SizeOverflow)?;
                    bytes.len()
                }
                PreparedChunk::Shared(chunk) => {
                    if !Arc::ptr_eq(&chunk.allocation.authority, &self.authority) {
                        return Err(SnapshotStoreError::ForeignChunk);
                    }
                    chunk.allocation.bytes.len()
                }
            };
            logical_bytes = logical_bytes
                .checked_add(length)
                .ok_or(SnapshotStoreError::SizeOverflow)?;
        }
        self.validate_shape(logical_bytes, prepared.chunks.len())?;
        self.validate_admission(new_bytes, new_allocations)?;
        let allocations = prepared
            .chunks
            .into_iter()
            .map(|chunk| match chunk {
                PreparedChunk::New(bytes) => self.allocate(bytes),
                PreparedChunk::Shared(chunk) => chunk.allocation,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let root = self.make_root(
            prepared.revision,
            logical_bytes,
            SnapshotPayload::Chunks(allocations),
        );
        let assembly_time = publication_started.elapsed();
        let exchange_started = Instant::now();
        let mut state = self.lock_state();
        if !self.authority.active.load(Ordering::Acquire) {
            state.retired.push(root);
            return Err(SnapshotStoreError::Invalidated);
        }
        if let Some(previous) = state.latest.replace(root) {
            state.retired.push(previous);
        }
        drop(state);
        let root_exchange_time = exchange_started.elapsed();
        Ok(self.measurement(
            prepared.revision,
            SnapshotStorageKind::ImmutableChunks,
            logical_bytes,
            new_bytes,
            new_allocations,
            prepared.preparation_time,
            assembly_time,
            root_exchange_time,
            publication_started.elapsed(),
        ))
    }

    /// Atomically pins the complete latest root.
    pub fn acquire_latest(&self) -> Result<Option<SnapshotLease>, SnapshotStoreError> {
        if !self.authority.active.load(Ordering::Acquire) {
            return Err(SnapshotStoreError::Invalidated);
        }
        let state = self.lock_state();
        if !self.authority.active.load(Ordering::Acquire) {
            return Err(SnapshotStoreError::Invalidated);
        }
        Ok(state.latest.as_ref().map(|root| SnapshotLease {
            root: Arc::clone(root),
            authority: Arc::clone(&self.authority),
        }))
    }

    /// Invalidates authority before retiring the root. Existing leases may keep
    /// bytes alive, but can no longer authorize facet work.
    pub fn invalidate(&self) {
        self.authority.active.store(false, Ordering::Release);
        let mut state = self.lock_state();
        if let Some(previous) = state.latest.take() {
            state.retired.push(previous);
        }
    }

    /// Removes unleased superseded roots from store ownership and destroys them
    /// after releasing the publication lock. Call this on a reclamation worker,
    /// since dropping a root may release many chunks.
    pub fn reclaim_retired(&self) -> usize {
        let mut retirement = self
            .retirement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            let RetirementState { spare, .. } = &mut *retirement;
            let mut state = self.lock_state();
            std::mem::swap(&mut state.retired, spare);
        }

        let RetirementState { spare, pinned } = &mut *retirement;
        spare.append(pinned);
        let examined = spare.len();
        let mut survivors = std::mem::take(pinned);
        for root in spare.drain(..) {
            if Arc::strong_count(&root) > 1 {
                survivors.push(root);
            }
        }
        *pinned = survivors;
        let reclaimed = examined - pinned.len();
        drop(retirement);
        self.reclaim_allocations();
        reclaimed
    }

    #[must_use]
    pub fn retention(&self) -> SnapshotRetention {
        SnapshotRetention {
            live_bytes: self.authority.accounting.bytes.load(Ordering::Acquire),
            live_allocations: self
                .authority
                .accounting
                .allocations
                .load(Ordering::Acquire),
            live_roots: self.authority.accounting.roots.load(Ordering::Acquire),
        }
    }

    fn validate_revision(
        &self,
        state: &StoreState,
        revision: u64,
    ) -> Result<(), SnapshotStoreError> {
        if !self.authority.active.load(Ordering::Acquire) {
            return Err(SnapshotStoreError::Invalidated);
        }
        if let Some(latest) = &state.latest
            && revision <= latest.revision
        {
            return Err(SnapshotStoreError::StaleRevision {
                attempted: revision,
                latest: latest.revision,
            });
        }
        Ok(())
    }

    const fn validate_shape(&self, bytes: usize, chunks: usize) -> Result<(), SnapshotStoreError> {
        if bytes > self.limits.max_snapshot_bytes {
            return Err(SnapshotStoreError::SnapshotBytes {
                actual: bytes,
                limit: self.limits.max_snapshot_bytes,
            });
        }
        if chunks > self.limits.max_chunks_per_snapshot {
            return Err(SnapshotStoreError::SnapshotChunks {
                actual: chunks,
                limit: self.limits.max_chunks_per_snapshot,
            });
        }
        Ok(())
    }

    fn validate_admission(
        &self,
        additional_bytes: usize,
        additional_allocations: usize,
    ) -> Result<(), SnapshotStoreError> {
        let retained = self.retention();
        let bytes = retained
            .live_bytes
            .checked_add(additional_bytes)
            .ok_or(SnapshotStoreError::SizeOverflow)?;
        if bytes > self.limits.max_live_bytes {
            return Err(SnapshotStoreError::LiveBytes {
                actual: bytes,
                limit: self.limits.max_live_bytes,
            });
        }
        let allocations = retained
            .live_allocations
            .checked_add(additional_allocations)
            .ok_or(SnapshotStoreError::SizeOverflow)?;
        if allocations > self.limits.max_live_allocations {
            return Err(SnapshotStoreError::LiveAllocations {
                actual: allocations,
                limit: self.limits.max_live_allocations,
            });
        }
        let roots = retained
            .live_roots
            .checked_add(1)
            .ok_or(SnapshotStoreError::SizeOverflow)?;
        if roots > self.limits.max_live_roots {
            return Err(SnapshotStoreError::LiveRoots {
                actual: roots,
                limit: self.limits.max_live_roots,
            });
        }
        Ok(())
    }

    fn allocate(&self, bytes: Vec<u8>) -> Arc<Allocation> {
        let length = bytes.len();
        self.allocate_with(length, || bytes)
    }

    fn allocate_with(&self, length: usize, prepare: impl FnOnce() -> Vec<u8>) -> Arc<Allocation> {
        self.authority
            .accounting
            .bytes
            .fetch_add(length, Ordering::AcqRel);
        self.authority
            .accounting
            .allocations
            .fetch_add(1, Ordering::AcqRel);
        let bytes = prepare();
        let allocation = Arc::new(Allocation {
            bytes: bytes.into_boxed_slice(),
            authority: Arc::clone(&self.authority),
        });
        self.allocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accepted
            .push(Arc::clone(&allocation));
        allocation
    }

    fn reclaim_allocations(&self) {
        let mut retirement = self
            .allocation_retirement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            let AllocationRetirementState { spare, .. } = &mut *retirement;
            let mut allocations = self
                .allocations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::swap(&mut allocations.accepted, spare);
        }
        let AllocationRetirementState { spare, pinned } = &mut *retirement;
        spare.append(pinned);
        let mut survivors = std::mem::take(pinned);
        for allocation in spare.drain(..) {
            if Arc::strong_count(&allocation) > 1 {
                survivors.push(allocation);
            }
        }
        *pinned = survivors;
        drop(retirement);
    }

    fn make_root(
        &self,
        revision: u64,
        logical_bytes: usize,
        payload: SnapshotPayload,
    ) -> Arc<SnapshotRoot> {
        self.authority
            .accounting
            .roots
            .fetch_add(1, Ordering::AcqRel);
        Arc::new(SnapshotRoot {
            revision,
            logical_bytes,
            payload,
            authority: Arc::clone(&self.authority),
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "measurement fields mirror distinct prototype stages"
    )]
    fn measurement(
        &self,
        revision: u64,
        kind: SnapshotStorageKind,
        logical_bytes: usize,
        newly_allocated_bytes: usize,
        newly_allocated_chunks: usize,
        preparation_time: Duration,
        assembly_time: Duration,
        root_exchange_time: Duration,
        publication_time: Duration,
    ) -> PublicationMeasurement {
        let retained = self.retention();
        PublicationMeasurement {
            revision,
            kind,
            logical_bytes,
            newly_allocated_bytes,
            newly_allocated_chunks,
            preparation_time,
            assembly_time,
            root_exchange_time,
            publication_time,
            live_bytes_after_publication: retained.live_bytes,
            live_allocations_after_publication: retained.live_allocations,
            live_roots_after_publication: retained.live_roots,
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, StoreState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_publish_gate(&self) -> MutexGuard<'_, ()> {
        self.publish_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A coherent pin of one complete root.
#[derive(Clone, Debug)]
pub struct SnapshotLease {
    root: Arc<SnapshotRoot>,
    authority: Arc<GenerationAuthority>,
}

impl SnapshotLease {
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.root.revision
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.authority.id
    }

    #[must_use]
    pub fn logical_bytes(&self) -> usize {
        self.root.logical_bytes
    }

    #[must_use]
    pub fn storage_kind(&self) -> SnapshotStorageKind {
        match &self.root.payload {
            SnapshotPayload::Copied(_) => SnapshotStorageKind::Copied,
            SnapshotPayload::Chunks(_) => SnapshotStorageKind::ImmutableChunks,
        }
    }

    #[must_use]
    pub fn is_authoritative(&self) -> bool {
        self.authority.active.load(Ordering::Acquire)
    }

    /// Copies the coherent root into a contiguous guest-transfer buffer.
    pub fn copy_for_guest(&self) -> Result<GuestTransfer, SnapshotStoreError> {
        if !self.is_authoritative() {
            return Err(SnapshotStoreError::Invalidated);
        }
        let started = Instant::now();
        let bytes = match &self.root.payload {
            SnapshotPayload::Copied(allocation) => allocation.bytes.to_vec(),
            SnapshotPayload::Chunks(chunks) => {
                let mut bytes = Vec::with_capacity(self.root.logical_bytes);
                for chunk in chunks.as_ref() {
                    bytes.extend_from_slice(&chunk.bytes);
                }
                bytes
            }
        };
        if !self.is_authoritative() {
            return Err(SnapshotStoreError::Invalidated);
        }
        Ok(GuestTransfer {
            bytes,
            transfer_time: started.elapsed(),
        })
    }

    /// Returns handles suitable for constructing a later chunked revision.
    #[must_use]
    pub fn immutable_chunks(&self) -> Vec<ImmutableChunk> {
        match &self.root.payload {
            SnapshotPayload::Copied(allocation) => vec![ImmutableChunk {
                allocation: Arc::clone(allocation),
            }],
            SnapshotPayload::Chunks(chunks) => chunks
                .iter()
                .map(|allocation| ImmutableChunk {
                    allocation: Arc::clone(allocation),
                })
                .collect(),
        }
    }

    /// Iterates immutable payload slices without allocating handle metadata.
    #[must_use]
    pub fn iter_chunks(&self) -> SnapshotChunkIter<'_> {
        let allocations = match &self.root.payload {
            SnapshotPayload::Copied(allocation) => SnapshotChunkAllocations::One(Some(allocation)),
            SnapshotPayload::Chunks(chunks) => SnapshotChunkAllocations::Many(chunks.iter()),
        };
        SnapshotChunkIter { allocations }
    }
}

enum SnapshotChunkAllocations<'a> {
    One(Option<&'a Arc<Allocation>>),
    Many(std::slice::Iter<'a, Arc<Allocation>>),
}

/// Zero-allocation iterator over one coherent snapshot's payload chunks.
pub struct SnapshotChunkIter<'a> {
    allocations: SnapshotChunkAllocations<'a>,
}

impl<'a> Iterator for SnapshotChunkIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.allocations {
            SnapshotChunkAllocations::One(allocation) => allocation
                .take()
                .map(|allocation| allocation.bytes.as_ref()),
            SnapshotChunkAllocations::Many(allocations) => allocations
                .next()
                .map(|allocation| allocation.bytes.as_ref()),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.allocations {
            SnapshotChunkAllocations::One(allocation) => {
                let length = usize::from(allocation.is_some());
                (length, Some(length))
            }
            SnapshotChunkAllocations::Many(allocations) => allocations.size_hint(),
        }
    }
}

impl ExactSizeIterator for SnapshotChunkIter<'_> {}

/// Explicit contiguous transfer measurement for a separate guest memory.
#[derive(Debug)]
pub struct GuestTransfer {
    pub bytes: Vec<u8>,
    pub transfer_time: Duration,
}
