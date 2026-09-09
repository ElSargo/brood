//! Component host adapter for bounded immutable facet-view publication.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use wasmtime::AsContext;
use wasmtime::StoreContextMut;
use wasmtime::component::{Linker, WasmList};

use crate::snapshot_reclaimer::ManagedSnapshotStore;
use crate::snapshot_store::{PublicationMeasurement, SnapshotLease, SnapshotStoreError};

pub const RENDER_PUBLICATION_INTERFACE: &str = "brood:plugin/render-publication@0.1.0";
pub const PUBLISH_FUNCTION: &str = "publish";

/// WIT `publication-error`; declaration order is canonical ABI order.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(enum)]
#[repr(u8)]
pub enum PublicationError {
    #[component(name = "unavailable")]
    Unavailable,
    #[component(name = "invalid-revision")]
    InvalidRevision,
    #[component(name = "limit-reached")]
    LimitReached,
    #[component(name = "invalidated")]
    Invalidated,
}

/// Shareable publication authority for one managed plugin generation.
#[derive(Debug)]
pub struct FacetPublication {
    snapshots: ManagedSnapshotStore,
    interest: AtomicBool,
    dispatch_input_sequence: AtomicU64,
    published_input: Mutex<Option<(u64, u64)>>,
    waker: Option<PublicationWake>,
}

struct PublicationWake(Arc<dyn Fn() + Send + Sync>);
impl std::fmt::Debug for PublicationWake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PublicationWake")
    }
}

impl FacetPublication {
    #[must_use]
    pub const fn new(snapshots: ManagedSnapshotStore) -> Self {
        Self {
            snapshots,
            interest: AtomicBool::new(false),
            dispatch_input_sequence: AtomicU64::new(0),
            published_input: Mutex::new(None),
            waker: None,
        }
    }

    #[must_use]
    pub fn with_waker(mut self, wake: impl Fn() + Send + Sync + 'static) -> Self {
        self.waker = Some(PublicationWake(Arc::new(wake)));
        self
    }

    pub fn publish(
        &self,
        revision: u64,
        view: &[u8],
    ) -> Result<PublicationMeasurement, PublicationError> {
        let measurement = self
            .snapshots
            .publish_copy_of(revision, view)
            .map_err(map_store_error)?;
        let input_sequence = self.dispatch_input_sequence.load(Ordering::Acquire);
        if let Ok(mut published) = self.published_input.lock() {
            *published = (input_sequence != 0).then_some((revision, input_sequence));
        }
        if !self.interest.swap(true, Ordering::AcqRel)
            && let Some(waker) = &self.waker
        {
            (waker.0)();
        }
        Ok(measurement)
    }

    pub fn acquire_latest(&self) -> Result<Option<SnapshotLease>, SnapshotStoreError> {
        self.snapshots.acquire_latest()
    }

    /// Associates publications performed by the current actor dispatch with
    /// the exact admitted facet input. Zero is reserved as "no facet input".
    #[doc(hidden)]
    pub fn set_dispatch_input_sequence(&self, input_sequence: Option<u64>) {
        self.dispatch_input_sequence
            .store(input_sequence.unwrap_or(0), Ordering::Release);
    }

    #[must_use]
    pub fn input_sequence_for_revision(&self, revision: u64) -> Option<u64> {
        self.published_input.lock().ok().and_then(|published| {
            published
                .as_ref()
                .filter(|(published_revision, _sequence)| *published_revision == revision)
                .map(|(_revision, sequence)| *sequence)
        })
    }

    pub fn invalidate(&self) {
        self.snapshots.invalidate();
    }

    /// Coalesces any number of successful publications since the previous take.
    #[must_use]
    pub fn take_interest(&self) -> bool {
        self.interest.swap(false, Ordering::AcqRel)
    }
}

/// Adds the publication interface to a component linker. The resolver is the
/// authority boundary: returning `None` denies the individual call.
pub fn add_to_linker<T, F>(linker: &mut Linker<T>, resolve: F) -> Result<()>
where
    T: 'static,
    F: for<'a> Fn(&mut StoreContextMut<'a, T>) -> Option<Arc<FacetPublication>>
        + Send
        + Sync
        + 'static,
{
    let mut interface = linker.instance(RENDER_PUBLICATION_INTERFACE)?;
    interface.func_wrap(
        PUBLISH_FUNCTION,
        move |mut caller: StoreContextMut<'_, T>, (revision, view): (u64, WasmList<u8>)| {
            let Some(publication) = resolve(&mut caller) else {
                return Ok((Err(PublicationError::Unavailable),));
            };
            let view = view.as_le_slice(caller.as_context());
            Ok((publication.publish(revision, view).map(|_| ()),))
        },
    )?;
    Ok(())
}

const fn map_store_error(error: SnapshotStoreError) -> PublicationError {
    match error {
        SnapshotStoreError::StaleRevision { .. } => PublicationError::InvalidRevision,
        SnapshotStoreError::SnapshotBytes { .. }
        | SnapshotStoreError::SnapshotChunks { .. }
        | SnapshotStoreError::LiveBytes { .. }
        | SnapshotStoreError::LiveAllocations { .. }
        | SnapshotStoreError::LiveRoots { .. }
        | SnapshotStoreError::SizeOverflow => PublicationError::LimitReached,
        SnapshotStoreError::Invalidated => PublicationError::Invalidated,
        SnapshotStoreError::ForeignChunk => PublicationError::Unavailable,
    }
}
