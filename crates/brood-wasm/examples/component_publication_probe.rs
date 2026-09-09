use std::{env, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use brood_wasm::{
    ComponentActor, ComponentActorConfig, ComponentActorEvent, FacetPublicationConfig,
    snapshot_reclaimer::{BackgroundSnapshotReclaimer, SnapshotReclaimerConfig},
    snapshot_store::SnapshotStoreLimits,
};

const SNAPSHOT_LIMITS: SnapshotStoreLimits = SnapshotStoreLimits {
    max_live_bytes: 4096,
    max_live_allocations: 8,
    max_live_roots: 8,
    max_snapshot_bytes: 1024,
    max_chunks_per_snapshot: 1,
};

#[tokio::main]
async fn main() -> Result<()> {
    let component_path = PathBuf::from(
        env::args_os()
            .nth(1)
            .context("usage: component_publication_probe <actor.component.wasm>")?,
    );
    let reclaimer = Arc::new(BackgroundSnapshotReclaimer::new(SnapshotReclaimerConfig {
        max_generations: 1,
        poll_interval: Duration::from_millis(1),
    })?);
    let config = ComponentActorConfig {
        facet_publication: Some(FacetPublicationConfig {
            reclaimer: Arc::clone(&reclaimer),
            limits: SNAPSHOT_LIMITS,
        }),
        epoch_tick_interval: Duration::from_millis(1),
        epoch_yield_ticks: 1,
        ..ComponentActorConfig::default()
    };
    let mut actor = ComponentActor::load(&component_path, config).await?;
    let publication = actor
        .facet_publication()
        .context("configured actor omitted publication authority")?;

    actor.try_send_message("publication-proof".into(), b"publication-dispatch".to_vec())?;
    wait_for_dispatch(&mut actor).await?;
    assert_latest(&publication, 1, b"actor-dispatch")?;
    ensure!(
        publication.take_interest(),
        "dispatch did not signal interest"
    );

    actor.try_send_message("publication-proof".into(), b"publication-worker".to_vec())?;
    ensure!(
        wait_for_worker(&mut actor, 910)
            .await
            .context("standalone publication worker")?
            == b"unavailable",
        "standalone worker unexpectedly obtained publication authority"
    );
    assert_latest(&publication, 1, b"actor-dispatch")?;

    tokio::time::timeout(Duration::from_secs(2), actor.shutdown()).await??;
    drop(actor);
    ensure!(
        matches!(
            publication.publish(3, b"after-shutdown"),
            Err(brood_wasm::facet_publication::PublicationError::Invalidated)
        ),
        "retained publication remained authoritative after actor shutdown"
    );
    println!("component publication authority probe passed");
    Ok(())
}

fn assert_latest(
    publication: &brood_wasm::facet_publication::FacetPublication,
    revision: u64,
    expected: &[u8],
) -> Result<()> {
    let lease = publication
        .acquire_latest()?
        .context("publication root is absent")?;
    ensure!(lease.revision() == revision, "wrong publication revision");
    let bytes = lease.iter_chunks().flatten().copied().collect::<Vec<_>>();
    ensure!(bytes == expected, "wrong publication bytes");
    Ok(())
}

async fn wait_for_dispatch(actor: &mut ComponentActor) -> Result<()> {
    loop {
        match tokio::time::timeout(Duration::from_secs(2), actor.recv_event())
            .await
            .context("timed out waiting for dispatcher")?
        {
            Some(ComponentActorEvent::DispatcherReturned) => return Ok(()),
            Some(ComponentActorEvent::DispatcherFailed { message }) => anyhow::bail!(message),
            Some(ComponentActorEvent::Stopped) | None => anyhow::bail!("actor stopped"),
            Some(_) => {}
        }
    }
}

async fn wait_for_worker(actor: &mut ComponentActor, expected: u64) -> Result<Vec<u8>> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), actor.recv_event())
            .await
            .context("timed out waiting for worker result")?
        {
            Some(ComponentActorEvent::WorkerFinished { worker, output })
                if worker.raw() == expected =>
            {
                return Ok(output);
            }
            Some(ComponentActorEvent::WorkerFailed { worker, message })
                if worker.raw() == expected =>
            {
                anyhow::bail!(message)
            }
            Some(ComponentActorEvent::Stopped) | None => anyhow::bail!("actor stopped"),
            Some(_) => {}
        }
    }
}
