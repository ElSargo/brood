use std::{env, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use manifold_wasm::{Preview1ActorCapabilities, Preview1CompatActor, Preview1WorkerConfig};

#[tokio::main]
async fn main() -> Result<()> {
    let module = PathBuf::from(
        env::args_os()
            .nth(1)
            .context("usage: non_killing_preview1_actor <module>")?,
    );
    let config = Preview1WorkerConfig {
        epoch_tick_interval: Duration::from_millis(1),
        actor_yield_ticks: 1,
        worker_yield_ticks: 1,
        ..Preview1WorkerConfig::default()
    };
    let capabilities = Preview1ActorCapabilities::default();
    let mut runaway = Preview1CompatActor::load(&module, capabilities.clone(), config).await?;
    let mut peer = Preview1CompatActor::load(&module, capabilities, config).await?;

    runaway.try_send_message("proof".to_owned(), b"spin-dispatch".to_vec())?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    if let Some(message) = runaway.try_take_failure() {
        bail!("runaway Preview 1 actor was killed after yielding: {message}");
    }
    runaway.try_send_message("proof".to_owned(), b"queued-after-quantum".to_vec())?;

    peer.try_send_message("proof".to_owned(), b"peer-progress".to_vec())?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while peer.stats().mailbox_dispatched == 0 {
            if let Some(message) = peer.try_take_failure() {
                bail!("Preview 1 peer dispatcher failed: {message}");
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("responsive Preview 1 peer was starved by runaway actor")??;

    tokio::time::timeout(Duration::from_secs(2), peer.shutdown())
        .await
        .context("responsive Preview 1 peer did not stop")??;
    drop(peer);
    tokio::time::timeout(Duration::from_secs(2), runaway.shutdown())
        .await
        .context("runaway Preview 1 actor did not stop explicitly")??;
    drop(runaway);

    println!(
        "PASS runaway Preview 1 actor yielded without termination, peer progressed, explicit stop worked"
    );
    Ok(())
}
