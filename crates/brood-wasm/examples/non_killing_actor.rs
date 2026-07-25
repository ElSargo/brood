use std::{env, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use brood_wasm::{ComponentActor, ComponentActorConfig, ComponentActorEvent};

#[tokio::main]
async fn main() -> Result<()> {
    let component = PathBuf::from(
        env::args_os()
            .nth(1)
            .context("usage: non_killing_actor <component>")?,
    );
    let config = ComponentActorConfig {
        epoch_tick_interval: Duration::from_millis(1),
        epoch_yield_ticks: 1,
        ..ComponentActorConfig::default()
    };
    eprintln!("load runaway actor");
    let mut runaway = ComponentActor::load(&component, config.clone())
        .await
        .with_context(|| format!("load runaway actor from {}", component.display()))?;
    eprintln!("load responsive peer");
    let mut peer = ComponentActor::load(&component, config)
        .await
        .with_context(|| format!("load peer actor from {}", component.display()))?;

    eprintln!("start runaway dispatcher");
    runaway.try_send_message("proof".to_owned(), b"spin-dispatch".to_vec())?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    eprintln!("runaway survived scheduler quanta");

    for event in std::iter::from_fn(|| runaway.try_recv_event()) {
        match event {
            ComponentActorEvent::DispatcherFailed { message } => {
                bail!("runaway actor was killed after yielding: {message}");
            }
            ComponentActorEvent::Stopped => {
                bail!("runaway actor stopped after yielding");
            }
            _ => {}
        }
    }
    runaway.try_send_message("proof".to_owned(), b"queued-after-quantum".to_vec())?;

    eprintln!("dispatch responsive peer");
    peer.try_send_message("proof".to_owned(), b"peer-progress".to_vec())?;
    tokio::time::timeout(Duration::from_secs(2), wait_for_dispatch(&mut peer))
        .await
        .context("responsive peer was starved by runaway actor")??;

    eprintln!("stop responsive peer");
    tokio::time::timeout(Duration::from_secs(2), peer.shutdown())
        .await
        .context("responsive peer did not stop")??;
    drop(peer);
    eprintln!("stop runaway actor");
    tokio::time::timeout(Duration::from_secs(2), runaway.shutdown())
        .await
        .context("runaway actor did not stop explicitly")??;
    drop(runaway);

    println!(
        "PASS runaway actor yielded without termination, peer progressed, explicit stop worked"
    );
    Ok(())
}

async fn wait_for_dispatch(actor: &mut ComponentActor) -> Result<()> {
    loop {
        match actor.recv_event().await {
            Some(ComponentActorEvent::DispatcherReturned) => return Ok(()),
            Some(ComponentActorEvent::DispatcherFailed { message }) => {
                bail!("peer dispatcher failed: {message}");
            }
            Some(ComponentActorEvent::Stopped) | None => {
                bail!("peer stopped before dispatch returned");
            }
            Some(_) => {}
        }
    }
}
