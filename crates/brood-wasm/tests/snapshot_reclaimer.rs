#![allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "unexpected setup or reclaimer outcomes must fail this invariant test"
)]

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use brood_wasm::snapshot_reclaimer::{
    BackgroundSnapshotReclaimer, SnapshotReclaimerConfig, SnapshotReclaimerError,
};
use brood_wasm::snapshot_store::{
    PreparedChunk, PreparedChunkedSnapshot, PreparedCopiedSnapshot, SnapshotStoreError,
    SnapshotStoreLimits,
};

const fn limits() -> SnapshotStoreLimits {
    SnapshotStoreLimits {
        max_live_bytes: 64,
        max_live_allocations: 4,
        max_live_roots: 4,
        max_snapshot_bytes: 64,
        max_chunks_per_snapshot: 4,
    }
}

#[test]
fn background_owner_retains_generation_until_lease_and_chunk_drain() {
    let reclaimer = BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 1,
        poll_interval: Duration::from_millis(1),
    })
    .unwrap();
    let store = reclaimer.register(7, limits()).unwrap();
    store
        .publish_chunks(PreparedChunkedSnapshot::new(
            1,
            vec![PreparedChunk::New(b"retained".to_vec())],
        ))
        .unwrap();
    let lease = store.acquire_latest().unwrap().unwrap();
    let chunk = lease.immutable_chunks().remove(0);
    drop(store);

    assert!(matches!(
        lease.copy_for_guest(),
        Err(SnapshotStoreError::Invalidated)
    ));
    assert_eq!(chunk.bytes(), b"retained");
    assert_eq!(reclaimer.active_generations(), 1);
    drop(lease);
    drop(chunk);

    let deadline = Instant::now() + Duration::from_secs(2);
    while reclaimer.active_generations() != 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(reclaimer.active_generations(), 0);
}

#[test]
fn generation_registration_is_bounded() {
    let reclaimer = BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 1,
        poll_interval: Duration::from_millis(1),
    })
    .unwrap();
    let first = reclaimer.register(1, limits()).unwrap();
    assert!(matches!(
        reclaimer.register(2, limits()),
        Err(SnapshotReclaimerError::GenerationCapacity { limit: 1 })
    ));
    drop(first);
}

#[test]
fn concurrent_registration_cannot_exceed_generation_capacity() {
    const CALLERS: usize = 12;
    const CAPACITY: usize = 3;
    let reclaimer = Arc::new(
        BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
            max_generations: CAPACITY,
            poll_interval: Duration::from_millis(1),
        })
        .unwrap(),
    );
    let barrier = Arc::new(Barrier::new(CALLERS));
    let mut callers = Vec::new();
    for generation in 0..CALLERS {
        let reclaimer = Arc::clone(&reclaimer);
        let barrier = Arc::clone(&barrier);
        callers.push(thread::spawn(move || {
            barrier.wait();
            reclaimer.register(u64::try_from(generation).unwrap(), limits())
        }));
    }

    let mut admitted = Vec::new();
    let mut rejected = 0usize;
    for caller in callers {
        match caller.join().unwrap() {
            Ok(store) => admitted.push(store),
            Err(SnapshotReclaimerError::GenerationCapacity { limit: CAPACITY }) => rejected += 1,
            Err(error) => panic!("unexpected registration error: {error}"),
        }
    }
    assert_eq!(admitted.len(), CAPACITY);
    assert_eq!(admitted.len() + rejected, CALLERS);
    assert_eq!(reclaimer.active_generations(), CAPACITY);
    drop(admitted);
}

#[test]
fn rejects_unusable_store_limits_before_registration() {
    let reclaimer = BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 1,
        poll_interval: Duration::from_millis(1),
    })
    .unwrap();
    let mut invalid = limits();
    invalid.max_live_allocations = 0;
    assert!(matches!(
        reclaimer.register(2, invalid),
        Err(SnapshotReclaimerError::InvalidStoreLimits)
    ));
    assert_eq!(reclaimer.active_generations(), 0);
}

#[test]
fn dropping_service_does_not_join_or_abandon_pinned_generation() {
    let reclaimer = BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 1,
        poll_interval: Duration::from_millis(1),
    })
    .unwrap();
    let store = reclaimer.register(3, limits()).unwrap();
    store
        .publish_copied(PreparedCopiedSnapshot::new(1, b"view".to_vec()))
        .unwrap();
    let lease = store.acquire_latest().unwrap().unwrap();

    let started = Instant::now();
    drop(reclaimer);
    assert!(started.elapsed() < Duration::from_millis(100));
    drop(store);
    assert!(!lease.is_authoritative());
    drop(lease);
}
