#![allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "unexpected setup, thread, or store outcomes must fail this invariant test"
)]

use std::sync::{Arc, Barrier};
use std::thread;

use brood_wasm::snapshot_store::{
    PreparedChunk, PreparedChunkedSnapshot, PreparedCopiedSnapshot, SnapshotStorageKind,
    SnapshotStore, SnapshotStoreError, SnapshotStoreLimits,
};

const fn limits(bytes: usize, allocations: usize, roots: usize) -> SnapshotStoreLimits {
    SnapshotStoreLimits {
        max_live_bytes: bytes,
        max_live_allocations: allocations,
        max_live_roots: roots,
        max_snapshot_bytes: bytes,
        max_chunks_per_snapshot: allocations,
    }
}

#[test]
fn overload_preserves_latest_and_accounts_for_a_pinned_predecessor() {
    let store = SnapshotStore::new(7, limits(8, 2, 2));
    store
        .publish_copied(PreparedCopiedSnapshot::new(1, vec![1; 4]))
        .unwrap();
    let pinned = store.acquire_latest().unwrap().unwrap();
    store
        .publish_copied(PreparedCopiedSnapshot::new(2, vec![2; 4]))
        .unwrap();
    assert_eq!(store.retention().live_bytes, 8);

    assert_eq!(
        store.publish_copied(PreparedCopiedSnapshot::new(3, vec![3; 4])),
        Err(SnapshotStoreError::LiveBytes {
            actual: 12,
            limit: 8,
        })
    );
    let latest = store.acquire_latest().unwrap().unwrap();
    assert_eq!(latest.revision(), 2);
    assert_eq!(latest.copy_for_guest().unwrap().bytes, vec![2; 4]);

    drop(pinned);
    assert_eq!(store.reclaim_retired(), 1);
    assert_eq!(store.retention().live_bytes, 4);
}

#[test]
fn immutable_chunks_share_unchanged_data_and_keep_revisions_coherent() {
    let store = SnapshotStore::new(2, limits(32, 4, 3));
    store
        .publish_chunks(PreparedChunkedSnapshot::new(
            1,
            vec![
                PreparedChunk::New(b"session".to_vec()),
                PreparedChunk::New(b"row-a".to_vec()),
            ],
        ))
        .unwrap();
    let old = store.acquire_latest().unwrap().unwrap();
    let shared = old.immutable_chunks().remove(0);
    let measurement = store
        .publish_chunks(PreparedChunkedSnapshot::new(
            2,
            vec![
                PreparedChunk::Shared(shared),
                PreparedChunk::New(b"row-b".to_vec()),
            ],
        ))
        .unwrap();

    assert_eq!(measurement.kind, SnapshotStorageKind::ImmutableChunks);
    assert_eq!(measurement.newly_allocated_bytes, 5);
    assert_eq!(measurement.newly_allocated_chunks, 1);
    assert_eq!(
        store
            .acquire_latest()
            .unwrap()
            .unwrap()
            .copy_for_guest()
            .unwrap()
            .bytes,
        b"sessionrow-b"
    );
    assert_eq!(old.copy_for_guest().unwrap().bytes, b"sessionrow-a");
}

#[test]
fn dropping_store_revokes_authority_while_a_lease_keeps_memory_alive() {
    let store = SnapshotStore::new(12, limits(16, 2, 2));
    store
        .publish_copied(PreparedCopiedSnapshot::new(1, vec![4; 8]))
        .unwrap();
    let lease = store.acquire_latest().unwrap().unwrap();
    drop(store);

    assert!(!lease.is_authoritative());
    assert!(matches!(
        lease.copy_for_guest(),
        Err(SnapshotStoreError::Invalidated)
    ));
}

#[test]
fn concurrent_publication_acquisition_and_reclamation_never_tear_a_root() {
    const REVISIONS: u64 = 120;
    const WIDTH: usize = 64;
    let store = Arc::new(SnapshotStore::new(
        21,
        SnapshotStoreLimits {
            max_live_bytes: WIDTH * 8,
            max_live_allocations: 8,
            max_live_roots: 8,
            max_snapshot_bytes: WIDTH,
            max_chunks_per_snapshot: 1,
        },
    ));
    store
        .publish_copied(PreparedCopiedSnapshot::new(1, vec![1; WIDTH]))
        .unwrap();
    let start = Arc::new(Barrier::new(3));

    let reader_store = Arc::clone(&store);
    let reader_start = Arc::clone(&start);
    let reader = thread::spawn(move || {
        reader_start.wait();
        let mut observations = 0usize;
        loop {
            let lease = match reader_store.acquire_latest() {
                Ok(Some(lease)) => lease,
                Err(SnapshotStoreError::Invalidated) => break,
                Ok(None) => continue,
                Err(error) => panic!("unexpected acquire error: {error}"),
            };
            let revision = lease.revision();
            let transfer = match lease.copy_for_guest() {
                Ok(transfer) => transfer,
                Err(SnapshotStoreError::Invalidated) => break,
                Err(error) => panic!("unexpected transfer error: {error}"),
            };
            assert_eq!(transfer.bytes.len(), WIDTH);
            assert!(
                transfer
                    .bytes
                    .iter()
                    .all(|byte| u64::from(*byte) == revision)
            );
            observations = observations.saturating_add(1);
        }
        observations
    });

    let reclaimer_store = Arc::clone(&store);
    let reclaimer_start = Arc::clone(&start);
    let reclaimer = thread::spawn(move || {
        reclaimer_start.wait();
        while reclaimer_store.acquire_latest().is_ok() {
            reclaimer_store.reclaim_retired();
            thread::yield_now();
        }
        reclaimer_store.reclaim_retired();
    });

    start.wait();
    for revision in 2..REVISIONS {
        loop {
            let prepared =
                PreparedCopiedSnapshot::new(revision, vec![u8::try_from(revision).unwrap(); WIDTH]);
            match store.publish_copied(prepared) {
                Ok(_) => break,
                Err(
                    SnapshotStoreError::LiveBytes { .. }
                    | SnapshotStoreError::LiveAllocations { .. }
                    | SnapshotStoreError::LiveRoots { .. },
                ) => thread::yield_now(),
                Err(error) => panic!("unexpected publication error: {error}"),
            }
        }
    }
    store.invalidate();
    assert!(reader.join().unwrap() > 0);
    reclaimer.join().unwrap();
}

#[test]
fn empty_chunks_still_consume_allocation_and_chunk_limits() {
    let store = SnapshotStore::new(30, limits(1, 1, 2));
    let first = store
        .publish_chunks(PreparedChunkedSnapshot::new(
            1,
            vec![PreparedChunk::New(Vec::new())],
        ))
        .unwrap();
    assert_eq!(first.newly_allocated_bytes, 0);
    assert_eq!(first.newly_allocated_chunks, 1);
    assert_eq!(store.retention().live_allocations, 1);
    assert_eq!(
        store.publish_chunks(PreparedChunkedSnapshot::new(
            2,
            vec![PreparedChunk::New(Vec::new())],
        )),
        Err(SnapshotStoreError::LiveAllocations {
            actual: 2,
            limit: 1,
        })
    );
    assert_eq!(store.acquire_latest().unwrap().unwrap().revision(), 1);
}

#[test]
fn stale_and_foreign_publications_leave_the_root_unchanged() {
    let store = SnapshotStore::new(40, limits(32, 4, 3));
    store
        .publish_chunks(PreparedChunkedSnapshot::new(
            2,
            vec![PreparedChunk::New(b"accepted".to_vec())],
        ))
        .unwrap();
    assert_eq!(
        store.publish_copied(PreparedCopiedSnapshot::new(2, b"stale".to_vec())),
        Err(SnapshotStoreError::StaleRevision {
            attempted: 2,
            latest: 2,
        })
    );

    let foreign_store = SnapshotStore::new(41, limits(32, 4, 3));
    foreign_store
        .publish_chunks(PreparedChunkedSnapshot::new(
            1,
            vec![PreparedChunk::New(b"foreign".to_vec())],
        ))
        .unwrap();
    let foreign = foreign_store
        .acquire_latest()
        .unwrap()
        .unwrap()
        .immutable_chunks()
        .remove(0);
    assert!(matches!(
        store.publish_chunks(PreparedChunkedSnapshot::new(
            3,
            vec![PreparedChunk::Shared(foreign)]
        )),
        Err(SnapshotStoreError::ForeignChunk)
    ));
    assert_eq!(
        store
            .acquire_latest()
            .unwrap()
            .unwrap()
            .copy_for_guest()
            .unwrap()
            .bytes,
        b"accepted"
    );
}

#[test]
fn exported_chunk_remains_charged_after_its_root_is_reclaimed() {
    let store = SnapshotStore::new(50, limits(32, 4, 3));
    store
        .publish_chunks(PreparedChunkedSnapshot::new(
            1,
            vec![PreparedChunk::New(b"held".to_vec())],
        ))
        .unwrap();
    let chunk = store
        .acquire_latest()
        .unwrap()
        .unwrap()
        .immutable_chunks()
        .remove(0);
    store
        .publish_copied(PreparedCopiedSnapshot::new(2, b"new".to_vec()))
        .unwrap();
    assert_eq!(store.reclaim_retired(), 1);
    assert_eq!(store.retention().live_bytes, 7);
    assert_eq!(chunk.bytes(), b"held");
    drop(chunk);
    store.reclaim_retired();
    assert_eq!(store.retention().live_bytes, 3);
}

#[test]
fn invalidate_racing_publication_never_leaves_authority() {
    let store = Arc::new(SnapshotStore::new(60, limits(64 * 1_024, 1_024, 1_024)));
    store
        .publish_copied(PreparedCopiedSnapshot::new(1, vec![1; 64]))
        .unwrap();
    let publisher_store = Arc::clone(&store);
    let publisher = thread::spawn(move || {
        for revision in 2..1_000 {
            if matches!(
                publisher_store.publish_copied(PreparedCopiedSnapshot::new(revision, vec![1; 64])),
                Err(SnapshotStoreError::Invalidated)
            ) {
                break;
            }
            publisher_store.reclaim_retired();
        }
    });
    store.invalidate();
    publisher.join().unwrap();
    assert!(matches!(
        store.acquire_latest(),
        Err(SnapshotStoreError::Invalidated)
    ));
    assert!(matches!(
        store.publish_copied(PreparedCopiedSnapshot::new(1_001, vec![1; 64])),
        Err(SnapshotStoreError::Invalidated)
    ));
}
