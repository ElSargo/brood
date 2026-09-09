use std::{env, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use brood_wasm::{
    ActorCompletionKind, ComponentActor, ComponentActorConfig, ComponentActorEvent,
    FilesystemGrant,
    facet_probe::{FacetProbeConfig, FacetProbeInput, run_facet_probe, verify_denied_imports},
};

#[tokio::main]
async fn main() -> Result<()> {
    let component = PathBuf::from(
        env::args_os()
            .nth(1)
            .context("usage: facet_probe <wasm-component-actor component>")?,
    );
    let capability = tempfile::tempdir().context("create actor probe capability")?;
    let actor_config = ComponentActorConfig {
        filesystem: Some(FilesystemGrant {
            host_path: capability.path().to_path_buf(),
            guest_path: ".".into(),
            writable: true,
        }),
        epoch_tick_interval: Duration::from_millis(1),
        epoch_yield_ticks: 1,
        ..ComponentActorConfig::default()
    };
    let mut associated = ComponentActor::load(&component, actor_config.clone()).await?;
    let mut responsive_peer = ComponentActor::load(&component, actor_config).await?;
    associated.try_send_message("facet-proof".into(), b"spawn-spin-worker".to_vec())?;
    tokio::time::timeout(
        Duration::from_secs(2),
        wait_for_worker_entry(&mut associated),
    )
    .await
    .context("worker guest did not reach its spin checkpoint")??;
    ensure!(
        std::fs::read(capability.path().join("spin-entered"))? == b"yes",
        "worker entry checkpoint contents differ"
    );
    associated.try_send_message("facet-proof".into(), b"spin-dispatch".to_vec())?;
    tokio::time::timeout(
        Duration::from_secs(2),
        wait_for_marker(capability.path().join("dispatch-spin-entered")),
    )
    .await
    .context("actor guest did not reach its spin checkpoint while worker remained active")??;
    ensure!(
        std::fs::read(capability.path().join("dispatch-spin-entered"))? == b"yes",
        "actor entry checkpoint contents differ"
    );

    let session_list = serde_json::to_vec(&serde_json::json!({
        "revision": 41,
        "sessions": (0..128).map(|id| serde_json::json!({
            "id": format!("session-{id}"), "title": format!("project {id}"), "active": id == 17
        })).collect::<Vec<_>>()
    }))?;
    let terminal_rows = [24_usize, 80, 240, 1000].into_iter().map(|rows| {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "revision": 1000 + rows,
            "rows": (0..rows).map(|row| format!("{row:04}: facet transfer row with changing terminal content λ界"))
                .collect::<Vec<_>>()
        }))?;
        Ok(FacetProbeInput { label: format!("terminal-rows-{rows}"), bytes })
    }).collect::<Result<Vec<_>>>()?;
    let mut inputs = vec![FacetProbeInput {
        label: "session-list-128".into(),
        bytes: session_list,
    }];
    inputs.extend(terminal_rows);

    responsive_peer.try_send_message("facet-proof".into(), b"peer-progress".to_vec())?;
    let facet = tokio::task::spawn_blocking(move || {
        run_facet_probe(
            include_bytes!("fixtures/facet-probe.wat"),
            inputs,
            FacetProbeConfig {
                epoch_deadline_ticks: 100,
                ..FacetProbeConfig::default()
            },
        )
    });
    let (peer_result, report) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_dispatch(&mut responsive_peer)
        ),
        async { facet.await.context("join facet probe task") }
    );
    peer_result??;
    let report = report??;
    ensure!(
        verify_denied_imports(include_bytes!("fixtures/facet-denied-import.wat")).is_err(),
        "denied-import fixture unexpectedly passed validation"
    );
    for timing in report.timings {
        println!(
            "{} input={}B output={}B copy_in={:?} guest={:?} copy_out={:?} fuel={} checksum={}",
            timing.label,
            timing.input_bytes,
            timing.output_bytes,
            timing.copy_in,
            timing.guest_call,
            timing.copy_out,
            timing.fuel_consumed,
            timing.output_checksum
        );
    }
    ensure!(
        associated.mailbox_stats().enqueued_messages >= 2,
        "busy associated actor did not retain its admitted mailbox work"
    );
    tokio::time::timeout(Duration::from_secs(2), responsive_peer.shutdown()).await??;
    drop(responsive_peer);
    tokio::time::timeout(Duration::from_secs(2), associated.shutdown()).await??;
    drop(associated);
    Ok(())
}

async fn wait_for_worker_entry(actor: &mut ComponentActor) -> Result<()> {
    loop {
        match actor.recv_event().await {
            Some(ComponentActorEvent::CompletionDelivered(completion))
                if matches!(
                    completion.kind,
                    ActorCompletionKind::Write { bytes_written: 3 }
                ) =>
            {
                return Ok(());
            }
            Some(
                ComponentActorEvent::WorkerFailed { message, .. }
                | ComponentActorEvent::DispatcherFailed { message },
            ) => bail!(message),
            Some(ComponentActorEvent::Stopped) | None => bail!("actor stopped before worker start"),
            Some(_) => {}
        }
    }
}

async fn wait_for_dispatch(actor: &mut ComponentActor) -> Result<()> {
    loop {
        match actor.recv_event().await {
            Some(ComponentActorEvent::DispatcherReturned) => return Ok(()),
            Some(ComponentActorEvent::DispatcherFailed { message }) => bail!(message),
            Some(ComponentActorEvent::Stopped) | None => bail!("peer stopped before dispatch"),
            Some(_) => {}
        }
    }
}

async fn wait_for_marker(path: PathBuf) -> Result<()> {
    loop {
        match std::fs::read(&path) {
            Ok(bytes) if bytes == b"yes" => return Ok(()),
            Ok(_) => bail!("actor spin checkpoint contents differ"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(error) => return Err(error).context("read actor spin checkpoint"),
        }
    }
}
