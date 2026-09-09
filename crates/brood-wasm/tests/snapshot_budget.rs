#![allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "unexpected setup, thread, or budget outcomes must fail this invariant test"
)]

use std::sync::{Arc, Barrier};
use std::thread;

use brood_wasm::snapshot_budget::{
    BudgetedBytes, ByteBudget, ByteBudgetError, ByteBudgetLimits, ByteBudgetRetention,
    CaptureBoundedError,
};
use brood_wasm::snapshot_store::{PreparedCopiedSnapshot, SnapshotStore, SnapshotStoreLimits};

const fn budget_limits(bytes: usize, allocations: usize) -> ByteBudgetLimits {
    ByteBudgetLimits {
        max_live_bytes: bytes,
        max_live_allocations: allocations,
        max_buffer_bytes: bytes,
    }
}

const fn store_limits(bytes: usize) -> SnapshotStoreLimits {
    SnapshotStoreLimits {
        max_live_bytes: bytes,
        max_live_allocations: 4,
        max_live_roots: 4,
        max_snapshot_bytes: bytes,
        max_chunks_per_snapshot: 4,
    }
}

#[test]
fn staging_reserves_before_copy_and_remains_charged_until_drop() {
    let budget = ByteBudget::new(1, budget_limits(8, 1));
    let buffer = BudgetedBytes::copy_from_slice(&budget, b"12345678").unwrap();
    assert_eq!(buffer.bytes().unwrap(), b"12345678");
    assert_eq!(
        budget.retention(),
        ByteBudgetRetention {
            live_bytes: 8,
            live_allocations: 1,
        }
    );
    assert_eq!(
        BudgetedBytes::copy_from_slice(&budget, b"x").unwrap_err(),
        ByteBudgetError::LiveBytes {
            actual: 9,
            limit: 8,
        }
    );
    drop(buffer);
    assert_eq!(budget.retention(), ByteBudgetRetention::default());
}

#[test]
fn empty_buffers_consume_allocation_capacity() {
    let budget = ByteBudget::new(2, budget_limits(1, 1));
    let empty = BudgetedBytes::copy_from_slice(&budget, b"").unwrap();
    assert_eq!(
        BudgetedBytes::copy_from_slice(&budget, b"").unwrap_err(),
        ByteBudgetError::LiveAllocations {
            actual: 2,
            limit: 1,
        }
    );
    drop(empty);
    assert_eq!(budget.retention(), ByteBudgetRetention::default());
}

#[test]
fn snapshot_transfer_is_bounded_and_pins_its_source_storage() {
    let store = SnapshotStore::new(3, store_limits(16));
    store
        .publish_copied(PreparedCopiedSnapshot::new(1, b"snapshot".to_vec()))
        .unwrap();
    let budget = ByteBudget::new(3, budget_limits(8, 1));
    let transfer =
        BudgetedBytes::copy_snapshot(&budget, store.acquire_latest().unwrap().unwrap()).unwrap();
    store
        .publish_copied(PreparedCopiedSnapshot::new(2, b"new".to_vec()))
        .unwrap();
    assert_eq!(store.reclaim_retired(), 0);
    assert_eq!(store.retention().live_bytes, 11);
    assert_eq!(transfer.bytes().unwrap(), b"snapshot");
    drop(transfer);
    assert_eq!(store.reclaim_retired(), 1);
    assert_eq!(store.retention().live_bytes, 3);
    assert_eq!(budget.retention(), ByteBudgetRetention::default());
}

#[test]
fn cancellation_revokes_transfer_without_releasing_either_charge() {
    let store = SnapshotStore::new(4, store_limits(8));
    store
        .publish_copied(PreparedCopiedSnapshot::new(1, b"retained".to_vec()))
        .unwrap();
    let budget = ByteBudget::new(4, budget_limits(8, 1));
    let transfer =
        BudgetedBytes::copy_snapshot(&budget, store.acquire_latest().unwrap().unwrap()).unwrap();
    budget.invalidate();

    assert!(!transfer.is_authoritative());
    assert_eq!(transfer.bytes(), Err(ByteBudgetError::Invalidated));
    assert_eq!(budget.retention().live_bytes, 8);
    assert_eq!(store.retention().live_bytes, 8);
    drop(transfer);
    assert_eq!(budget.retention(), ByteBudgetRetention::default());
}

#[test]
fn rejected_reservation_does_not_modify_the_snapshot_root() {
    let store = SnapshotStore::new(5, store_limits(8));
    store
        .publish_copied(PreparedCopiedSnapshot::new(9, b"current".to_vec()))
        .unwrap();
    let budget = ByteBudget::new(5, budget_limits(3, 1));
    assert_eq!(
        BudgetedBytes::copy_snapshot(&budget, store.acquire_latest().unwrap().unwrap())
            .unwrap_err(),
        ByteBudgetError::BufferBytes {
            actual: 7,
            limit: 3,
        }
    );
    let latest = store.acquire_latest().unwrap().unwrap();
    assert_eq!(latest.revision(), 9);
    assert_eq!(latest.copy_for_guest().unwrap().bytes, b"current");
    assert_eq!(budget.retention(), ByteBudgetRetention::default());
}

#[test]
fn concurrent_reservations_never_exceed_the_limit() {
    const THREADS: usize = 16;
    const BYTES: usize = 64;
    let budget = Arc::new(ByteBudget::new(6, budget_limits(BYTES * 4, 4)));
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut threads = Vec::new();
    for _ in 0..THREADS {
        let budget = Arc::clone(&budget);
        let barrier = Arc::clone(&barrier);
        threads.push(thread::spawn(move || {
            barrier.wait();
            BudgetedBytes::copy_from_slice(&budget, &[7; BYTES])
        }));
    }
    let mut admitted = Vec::new();
    let mut rejected = 0usize;
    for handle in threads {
        match handle.join().unwrap() {
            Ok(buffer) => admitted.push(buffer),
            Err(ByteBudgetError::LiveBytes { .. } | ByteBudgetError::LiveAllocations { .. }) => {
                rejected += 1;
            }
            Err(error) => panic!("unexpected reservation error: {error}"),
        }
    }
    assert!(admitted.len() <= 4);
    assert_eq!(admitted.len() + rejected, THREADS);
    assert_eq!(budget.retention().live_bytes, admitted.len() * BYTES);
    drop(admitted);
    assert_eq!(budget.retention(), ByteBudgetRetention::default());
}

#[test]
fn transfer_rejects_a_foreign_generation_before_reserving() {
    let store = SnapshotStore::new(7, store_limits(8));
    store
        .publish_copied(PreparedCopiedSnapshot::new(1, b"foreign".to_vec()))
        .unwrap();
    let budget = ByteBudget::new(8, budget_limits(8, 1));
    assert_eq!(
        BudgetedBytes::copy_snapshot(&budget, store.acquire_latest().unwrap().unwrap())
            .unwrap_err(),
        ByteBudgetError::ForeignGeneration {
            snapshot: 7,
            budget: 8,
        }
    );
    assert_eq!(budget.retention(), ByteBudgetRetention::default());
}

#[test]
fn last_budget_owner_drop_revokes_held_buffers() {
    let budget = ByteBudget::new(9, budget_limits(8, 1));
    let buffer = BudgetedBytes::copy_from_slice(&budget, b"retained").unwrap();
    let other_owner = budget.clone();
    drop(budget);
    assert_eq!(buffer.bytes().unwrap(), b"retained");
    drop(other_owner);
    assert!(matches!(buffer.bytes(), Err(ByteBudgetError::Invalidated)));
}

#[test]
fn concurrent_last_owner_drop_revokes_without_waiting_for_buffer_drop() {
    let budget = ByteBudget::new(10, budget_limits(8, 1));
    let buffer = BudgetedBytes::copy_from_slice(&budget, b"retained").unwrap();
    let dropper = thread::spawn(move || drop(budget));
    dropper.join().unwrap();
    assert!(!buffer.is_authoritative());
    assert!(matches!(buffer.bytes(), Err(ByteBudgetError::Invalidated)));
}

#[test]
fn bounded_capture_does_not_run_producer_when_admission_fails() {
    let budget = ByteBudget::new(11, budget_limits(4, 1));
    let held = BudgetedBytes::copy_from_slice(&budget, b"held").unwrap();
    let mut ran = false;
    let result = BudgetedBytes::capture_bounded(&budget, 1, || {
        ran = true;
        Ok::<_, &'static str>(vec![1])
    });
    assert!(matches!(
        result,
        Err(CaptureBoundedError::Budget(
            ByteBudgetError::LiveBytes { .. }
        ))
    ));
    assert!(!ran);
    drop(held);
}

#[test]
fn bounded_capture_releases_reservation_after_producer_failure() {
    let budget = ByteBudget::new(12, budget_limits(8, 1));
    let result = BudgetedBytes::capture_bounded(&budget, 8, || Err::<Vec<u8>, _>("lifting failed"));
    assert!(matches!(
        result,
        Err(CaptureBoundedError::Producer("lifting failed"))
    ));
    assert_eq!(budget.retention(), ByteBudgetRetention::default());
}

#[test]
fn bounded_capture_keeps_full_bound_charged_and_checks_owned_capacity() {
    let budget = ByteBudget::new(13, budget_limits(16, 2));
    let buffer =
        BudgetedBytes::capture_bounded(&budget, 8, || Ok::<_, &'static str>(Vec::with_capacity(8)))
            .unwrap();
    assert_eq!(buffer.len(), 0);
    assert_eq!(budget.retention().live_bytes, 8);
    assert!(matches!(
        BudgetedBytes::capture_bounded(&budget, 4, || {
            Ok::<_, &'static str>(Vec::with_capacity(5))
        }),
        Err(CaptureBoundedError::ProducedBufferTooLarge {
            length: 0,
            capacity: 5,
            limit: 4,
        })
    ));
    assert_eq!(budget.retention().live_bytes, 8);
    drop(buffer);
    assert_eq!(budget.retention(), ByteBudgetRetention::default());
}
