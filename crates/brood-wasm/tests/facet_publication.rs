#![allow(
    clippy::unwrap_used,
    reason = "unexpected component setup or publication outcomes must fail this integration test"
)]

use std::sync::Arc;
use std::time::Duration;

use brood_wasm::facet_publication::{FacetPublication, PublicationError, add_to_linker};
use brood_wasm::snapshot_reclaimer::{BackgroundSnapshotReclaimer, SnapshotReclaimerConfig};
use brood_wasm::snapshot_store::SnapshotStoreLimits;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

type PublishFunc = wasmtime::component::TypedFunc<(u64, Vec<u8>), (Result<(), PublicationError>,)>;

const COMPONENT: &str = r#"
(component
  (type $publication-instance (instance
    (type $publication-error (enum "unavailable" "invalid-revision" "limit-reached" "invalidated"))
    (export "publication-error" (type $publication-error-export (eq $publication-error)))
    (export "publish" (func
      (param "revision" u64)
      (param "view" (list u8))
      (result (result (error $publication-error-export)))))))
  (import "brood:plugin/render-publication@0.1.0"
    (instance $publication (type $publication-instance)))
  (alias export $publication "publication-error" (type $publication-error))
  (alias export $publication "publish" (func $publish-import))
  (core module $storage
    (memory (export "memory") 1)
    (global $next (mut i32) (i32.const 8))
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $result i32)
      global.get $next
      local.tee $result
      local.get 3
      i32.add
      global.set $next
      local.get $result))
  (core instance $storage-instance (instantiate $storage))
  (alias core export $storage-instance "memory" (core memory $memory))
  (alias core export $storage-instance "realloc" (core func $realloc))
  (core func $lowered-publish
    (canon lower (func $publish-import) (memory $memory)))
  (core instance $lowered-instance
    (export "publish" (func $lowered-publish)))
  (core module $adapter
    (import "host" "publish" (func $publish (param i64 i32 i32 i32)))
    (func (export "publish") (param i64 i32 i32) (result i32)
      local.get 0
      local.get 1
      local.get 2
      i32.const 0
      call $publish
      i32.const 0))
  (core instance $adapter-instance (instantiate $adapter
    (with "host" (instance $lowered-instance))))
  (alias core export $adapter-instance "publish" (core func $adapter-publish))
  (type $publish-export (func
    (param "revision" u64)
    (param "view" (list u8))
    (result (result (error $publication-error)))))
  (func $publish-export (type $publish-export)
    (canon lift (core func $adapter-publish) (memory $memory) (realloc $realloc)))
  (export "publish" (func $publish-export))
)
"#;

#[derive(Default)]
struct TestState {
    publication: Option<Arc<FacetPublication>>,
    authorized: bool,
}

const fn limits(bytes: usize) -> SnapshotStoreLimits {
    SnapshotStoreLimits {
        max_live_bytes: bytes,
        max_live_allocations: 4,
        max_live_roots: 4,
        max_snapshot_bytes: bytes,
        max_chunks_per_snapshot: 1,
    }
}

fn publication(bytes: usize) -> (BackgroundSnapshotReclaimer, Arc<FacetPublication>) {
    let reclaimer = BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 1,
        poll_interval: Duration::from_millis(1),
    })
    .unwrap();
    let snapshots = reclaimer.register(9, limits(bytes)).unwrap();
    (reclaimer, Arc::new(FacetPublication::new(snapshots)))
}

fn component_store(
    publication: Arc<FacetPublication>,
    authorized: bool,
) -> (Store<TestState>, PublishFunc) {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).unwrap();
    let component = Component::new(&engine, COMPONENT).unwrap();
    let mut linker: Linker<TestState> = Linker::new(&engine);
    add_to_linker(&mut linker, |caller| {
        caller
            .data()
            .authorized
            .then(|| caller.data().publication.as_ref().map(Arc::clone))
            .flatten()
    })
    .unwrap();
    let mut store = Store::new(
        &engine,
        TestState {
            publication: Some(publication),
            authorized,
        },
    );
    let instance = linker.instantiate(&mut store, &component).unwrap();
    let publish = instance
        .get_typed_func::<(u64, Vec<u8>), (Result<(), PublicationError>,)>(&mut store, "publish")
        .unwrap();
    (store, publish)
}

#[test]
fn real_component_publishes_owned_coherent_views_and_coalesces_interest() {
    let (_reclaimer, publication) = publication(32);
    let (mut store, publish) = component_store(Arc::clone(&publication), true);
    assert_eq!(
        publish.call(&mut store, (1, b"first".to_vec())).unwrap().0,
        Ok(())
    );
    let first = publication.acquire_latest().unwrap().unwrap();
    assert!(publication.take_interest());
    assert!(!publication.take_interest());

    assert_eq!(
        publish.call(&mut store, (2, b"second".to_vec())).unwrap().0,
        Ok(())
    );
    assert_eq!(first.copy_for_guest().unwrap().bytes, b"first");
    assert_eq!(
        publication
            .acquire_latest()
            .unwrap()
            .unwrap()
            .copy_for_guest()
            .unwrap()
            .bytes,
        b"second"
    );
    assert!(publication.take_interest());
}

#[test]
fn stale_and_limited_component_publications_preserve_latest_and_interest() {
    let (_reclaimer, publication) = publication(8);
    let (mut store, publish) = component_store(Arc::clone(&publication), true);
    assert_eq!(
        publish.call(&mut store, (2, b"root".to_vec())).unwrap().0,
        Ok(())
    );
    assert!(publication.take_interest());
    assert_eq!(
        publish.call(&mut store, (2, b"stale".to_vec())).unwrap().0,
        Err(PublicationError::InvalidRevision)
    );
    assert_eq!(
        publish
            .call(&mut store, (3, b"too-large".to_vec()))
            .unwrap()
            .0,
        Err(PublicationError::LimitReached)
    );
    assert!(!publication.take_interest());
    let latest = publication.acquire_latest().unwrap().unwrap();
    assert_eq!(latest.revision(), 2);
    assert_eq!(latest.copy_for_guest().unwrap().bytes, b"root");
}

#[test]
fn resolver_denies_unauthorized_component_call() {
    let (_reclaimer, publication) = publication(16);
    let (mut store, publish) = component_store(Arc::clone(&publication), false);
    assert_eq!(
        publish.call(&mut store, (1, b"denied".to_vec())).unwrap().0,
        Err(PublicationError::Unavailable)
    );
    assert!(!publication.take_interest());
    assert!(publication.acquire_latest().unwrap().is_none());
}

#[test]
fn publication_wakes_once_until_interest_is_taken() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let reclaimer = BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 1,
        poll_interval: Duration::from_millis(1),
    })?;
    let count = Arc::new(AtomicUsize::new(0));
    let wake_count = Arc::clone(&count);
    let publication =
        FacetPublication::new(reclaimer.register(9, limits(64))?).with_waker(move || {
            wake_count.fetch_add(1, Ordering::AcqRel);
        });
    publication
        .publish(1, b"first")
        .map_err(|e| anyhow::anyhow!("publication: {e:?}"))?;
    publication
        .publish(2, b"second")
        .map_err(|e| anyhow::anyhow!("publication: {e:?}"))?;
    anyhow::ensure!(count.load(Ordering::Acquire) == 1);
    anyhow::ensure!(publication.take_interest());
    publication
        .publish(3, b"third")
        .map_err(|e| anyhow::anyhow!("publication: {e:?}"))?;
    anyhow::ensure!(count.load(Ordering::Acquire) == 2);
    anyhow::ensure!(publication.publish(3, b"stale").is_err());
    anyhow::ensure!(count.load(Ordering::Acquire) == 2);
    Ok(())
}

#[test]
fn publication_input_metadata_is_exact_and_failed_publication_cannot_advance_it() {
    let (_reclaimer, publication) = publication(64);
    publication.set_dispatch_input_sequence(Some(7));
    assert!(publication.publish(1, b"first").is_ok());
    assert_eq!(publication.input_sequence_for_revision(1), Some(7));
    assert_eq!(publication.input_sequence_for_revision(2), None);

    publication.set_dispatch_input_sequence(Some(8));
    assert!(matches!(
        publication.publish(2, &[b'x'; 65]),
        Err(PublicationError::LimitReached)
    ));
    assert_eq!(publication.input_sequence_for_revision(1), Some(7));
    assert_eq!(publication.input_sequence_for_revision(2), None);

    publication.set_dispatch_input_sequence(None);
    let plain = publication.publish(2, b"plain");
    assert!(plain.is_ok(), "plain publication failed: {plain:?}");
    assert_eq!(publication.input_sequence_for_revision(2), None);
}
