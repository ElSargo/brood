use std::{env, fs, io::Write, path::PathBuf, process::Command, time::Duration};

use anyhow::{Context, Result, ensure};
use manifold_wasm::{
    ActorCompletionKind, ComponentActor, ComponentActorConfig, ComponentActorEvent,
    FilesystemGrant, WorkerId,
};
use tokio::time::{sleep, timeout};

#[tokio::main]
async fn main() -> Result<()> {
    let component = PathBuf::from(
        env::args_os()
            .nth(1)
            .context("usage: actor_worker_workload <component>")?,
    );
    let root = tempfile::tempdir().context("create workload capability root")?;
    create_fifo(&root.path().join("input.pipe"))?;
    create_fifo(&root.path().join("cancel.pipe"))?;
    create_fifo(&root.path().join("duplicate.pipe"))?;
    fs::write(root.path().join("completion.txt"), b"completion-data")?;

    let mut actor = ComponentActor::load(
        &component,
        ComponentActorConfig {
            filesystem: Some(FilesystemGrant {
                host_path: root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: true,
            }),
            max_workers: 1,
            dispatcher_deadline: Duration::from_millis(100),
            ..ComponentActorConfig::default()
        },
    )
    .await?;
    eprintln!("component-workload: actor loaded");

    let worker = actor.try_spawn_worker(b"input.pipe".to_vec())?;
    wait_for_worker_started(&mut actor, worker).await?;
    wait_for_dispatch_and_pending_io(&mut actor, "input.pipe").await?;
    eprintln!("component-workload: worker pending");

    actor.try_send_message("workload".to_string(), b"ping".to_vec())?;
    timeout(Duration::from_millis(100), wait_for_dispatch(&mut actor))
        .await
        .context("actor dispatcher did not answer while worker read was pending")??;

    let worker_still_pending = timeout(
        Duration::from_millis(20),
        wait_for_worker(&mut actor, worker),
    )
    .await
    .is_err();
    ensure!(
        worker_still_pending,
        "worker read completed before FIFO input was supplied"
    );

    let mut writer = fs::OpenOptions::new()
        .write(true)
        .open(root.path().join("input.pipe"))?;
    writer.write_all(b"worker-data")?;
    drop(writer);
    let output = timeout(Duration::from_secs(1), wait_for_worker(&mut actor, worker))
        .await
        .context("worker did not resume after FIFO input")??;
    ensure!(output == b"worker-data", "worker returned unexpected bytes");
    eprintln!("component-workload: worker resumed");

    actor.try_send_message("workload".to_string(), b"completion-read".to_vec())?;
    wait_for_dispatch(&mut actor).await?;
    let completion = timeout(Duration::from_secs(1), actor.recv_completion())
        .await
        .context("completion read timed out")?
        .context("completion lane closed")?;
    ensure!(
        completion.operation.generation() == actor.generation()
            && completion.operation.sequence() != 0,
        "guest-submitted completion has an invalid operation id"
    );
    ensure!(
        matches!(
            completion.kind,
            ActorCompletionKind::Read { ref bytes } if bytes == b"completion-data"
        ),
        "completion read returned unexpected data"
    );
    // The same typed completion is delivered to the guest dispatcher. The
    // example reacts by spawning an echo worker carrying those bytes.
    let guest_completion_output = timeout(
        Duration::from_secs(1),
        wait_for_worker(&mut actor, WorkerId::from_raw(99)),
    )
    .await
    .context("guest did not react to typed completion event")??;
    ensure!(
        guest_completion_output == b"completion-data",
        "guest completion event carried unexpected bytes"
    );
    eprintln!("component-workload: completion delivered");

    actor.try_send_message("workload".to_string(), b"notification-start".to_vec())?;
    wait_for_dispatch(&mut actor).await?;
    let notification_worker = WorkerId::from_raw(800);
    wait_for_worker_started(&mut actor, notification_worker).await?;
    wait_for_dispatch(&mut actor).await?;
    actor.try_send_message("workload".to_string(), b"ping".to_vec())?;
    timeout(Duration::from_millis(100), wait_for_dispatch(&mut actor))
        .await
        .context("actor dispatcher stalled behind worker wait-any")??;
    ensure!(
        timeout(
            Duration::from_millis(20),
            wait_for_worker(&mut actor, notification_worker),
        )
        .await
        .is_err(),
        "notification worker finished before its shared signal"
    );
    actor.try_send_message("workload".to_string(), b"notification-signal".to_vec())?;
    wait_for_dispatch(&mut actor).await?;
    wait_for_component_notification_flow(&mut actor).await?;
    eprintln!("component-workload: shared notification select completed");

    actor.try_send_message(
        "workload".to_string(),
        b"notification-cancel-start".to_vec(),
    )?;
    wait_for_dispatch(&mut actor).await?;
    let notification_cancel_worker = WorkerId::from_raw(802);
    wait_for_worker_started(&mut actor, notification_cancel_worker).await?;
    wait_for_dispatch(&mut actor).await?;
    sleep(Duration::from_millis(20)).await;
    ensure!(
        actor.coordination_stats().wait_registrations == 1,
        "notification worker did not suspend in the host wait set"
    );
    actor.try_cancel_worker(notification_cancel_worker)?;
    wait_for_worker_cancelled(&mut actor, notification_cancel_worker).await?;
    wait_for_dispatch(&mut actor).await?;
    ensure!(
        actor.coordination_stats().wait_registrations == 0,
        "cancelled worker leaked a wait registration"
    );
    actor.try_send_message("workload".to_string(), b"ping-after-select-cancel".to_vec())?;
    wait_for_dispatch(&mut actor).await?;
    eprintln!("component-workload: notification wait cancelled cleanly");

    let cancelled_worker = actor.try_spawn_worker(b"cancel.pipe".to_vec())?;
    wait_for_worker_started(&mut actor, cancelled_worker).await?;
    wait_for_dispatch(&mut actor).await?;
    actor.try_cancel_worker(cancelled_worker)?;
    wait_for_worker_cancelled(&mut actor, cancelled_worker).await?;
    wait_for_dispatch(&mut actor).await?;

    actor.try_send_message("workload".to_string(), b"ping-after-cancel".to_vec())?;
    timeout(Duration::from_millis(100), wait_for_dispatch(&mut actor))
        .await
        .context("actor stopped responding after worker cancellation")??;

    let fifo = root.path().join("cancel.pipe");
    let write_command = format!("printf cancelled-data > {}", fifo.display());
    let _ = Command::new("timeout")
        .args(["0.2", "sh", "-c", &write_command])
        .status()?;
    ensure!(
        timeout(
            Duration::from_millis(75),
            wait_for_worker(&mut actor, cancelled_worker),
        )
        .await
        .is_err(),
        "cancelled worker delivered a late result while actor remained alive"
    );
    sleep(Duration::from_millis(50)).await;
    ensure!(
        !root.path().join("cancelled-worker-survived").exists(),
        "guest task performed I/O after its terminal cancellation event"
    );
    eprintln!("component-workload: I/O worker cancelled");

    actor.try_send_message("workload".to_string(), b"spawn-duplicate-workers".to_vec())?;
    wait_for_dispatch(&mut actor).await?;
    let duplicate_worker = WorkerId::from_raw(700);
    wait_for_worker_started(&mut actor, duplicate_worker).await?;
    let duplicate_failure = wait_for_worker_failed(&mut actor, duplicate_worker).await?;
    ensure!(
        duplicate_failure.contains("already active"),
        "duplicate worker failed for the wrong reason: {duplicate_failure}"
    );
    // The started and failed observer events are enqueued while handling the
    // mailbox dispatch, before either lifecycle event is dispatched back into
    // the guest. Consume those two dispatches before requesting cancellation
    // so the terminal cancellation dispatch can be verified independently.
    for _ in 0..2 {
        wait_for_dispatch(&mut actor).await?;
    }
    actor.try_cancel_worker(duplicate_worker)?;
    wait_for_worker_cancelled(&mut actor, duplicate_worker).await?;
    wait_for_dispatch(&mut actor).await?;
    actor.try_send_message("workload".to_string(), b"ping-after-admission".to_vec())?;
    wait_for_dispatch(&mut actor).await?;
    eprintln!("component-workload: duplicate worker failure was typed");

    let spinner = actor.try_spawn_worker(b"__spin__".to_vec())?;
    eprintln!("component-workload: CPU worker spawn queued");
    wait_for_worker_started(&mut actor, spinner).await?;
    eprintln!("component-workload: CPU worker started");
    wait_for_dispatch(&mut actor).await?;
    eprintln!("component-workload: CPU worker started event dispatched");
    let checkpoint = timeout(Duration::from_secs(1), actor.recv_completion())
        .await
        .context("CPU worker did not enter its spin branch")?
        .context("completion lane closed")?;
    ensure!(
        matches!(
            checkpoint.kind,
            ActorCompletionKind::Write { bytes_written: 3 }
        ),
        "CPU worker checkpoint write failed"
    );
    eprintln!("component-workload: CPU worker checkpoint");
    // Consume dispatch of the checkpoint completion, then give the worker
    // several engine epochs in which to enter and remain in its CPU loop.
    wait_for_dispatch(&mut actor).await?;
    sleep(Duration::from_millis(20)).await;
    let capacity_worker = actor.try_spawn_worker(b"input.pipe".to_vec())?;
    let capacity_failure = wait_for_worker_failed(&mut actor, capacity_worker).await?;
    ensure!(
        capacity_failure.contains("limit reached"),
        "capacity worker failed for the wrong reason: {capacity_failure}"
    );
    wait_for_dispatch(&mut actor).await?;
    actor.try_send_message("workload".to_string(), b"ping-during-spin".to_vec())?;
    timeout(Duration::from_millis(100), wait_for_dispatch(&mut actor))
        .await
        .context("actor dispatcher stalled behind CPU-running worker")??;
    eprintln!("component-workload: CPU worker cooperatively yielded");
    actor.try_cancel_worker(spinner)?;
    wait_for_worker_cancelled(&mut actor, spinner).await?;
    wait_for_dispatch(&mut actor).await?;
    eprintln!("component-workload: CPU worker cancelled");

    actor.shutdown().await?;
    eprintln!("component-workload: actor shutdown");
    while let Ok(Some(event)) = timeout(Duration::from_millis(20), actor.recv_event()).await {
        ensure!(
            !matches!(event, ComponentActorEvent::WorkerFinished { worker, .. } if worker == cancelled_worker),
            "cancelled actor delivered a late worker completion"
        );
    }

    let mut deadline_actor = ComponentActor::load(
        &component,
        ComponentActorConfig {
            filesystem: Some(FilesystemGrant {
                host_path: root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: true,
            }),
            dispatcher_deadline: Duration::from_millis(30),
            ..ComponentActorConfig::default()
        },
    )
    .await?;
    eprintln!("component-workload: deadline actor loaded");
    deadline_actor.try_send_message("workload".to_string(), b"spin-dispatch".to_vec())?;
    timeout(
        Duration::from_millis(250),
        wait_for_dispatch_failure(&mut deadline_actor),
    )
    .await
    .context("non-yielding dispatcher deadline was not enforced")??;
    eprintln!("component-workload: dispatcher deadline fired");
    ensure!(
        timeout(Duration::from_secs(1), deadline_actor.shutdown())
            .await
            .context("failed dispatcher prevented structured shutdown")?
            .is_err(),
        "failed dispatcher unexpectedly reported a successful shutdown"
    );
    drop(deadline_actor);

    println!(
        "component_actor_workload=ok pending_io_observed=true ping_while_worker_pending=true worker_resumed=true completion_io=true notification_wait_any=true notification_coalesced=true actor_progress_during_wait=true notification_wait_cancelled=true cancellation_guest_terminal=true worker_admission_typed=true cpu_worker_cooperative_yield=true dispatcher_deadline_enforced=true"
    );
    Ok(())
}

fn create_fifo(path: &std::path::Path) -> Result<()> {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .with_context(|| format!("run mkfifo for {}", path.display()))?;
    ensure!(status.success(), "mkfifo failed for {}", path.display());
    Ok(())
}

async fn wait_for_worker_started(actor: &mut ComponentActor, expected: WorkerId) -> Result<()> {
    loop {
        let event = next_event(actor, "waiting for worker start").await?;
        if matches!(event, ComponentActorEvent::WorkerStarted { worker } if worker == expected) {
            return Ok(());
        }
    }
}

async fn wait_for_component_notification_flow(actor: &mut ComponentActor) -> Result<()> {
    let waiting_worker = WorkerId::from_raw(800);
    let acknowledgement_worker = WorkerId::from_raw(801);
    let mut saw_notification = false;
    let mut saw_waiting_result = false;
    let mut saw_acknowledgement = false;
    timeout(Duration::from_secs(1), async {
        while !(saw_notification && saw_waiting_result && saw_acknowledgement) {
            match actor
                .recv_event()
                .await
                .context("component event lane closed during notification flow")?
            {
                ComponentActorEvent::NotificationDelivered {
                    generation: 3,
                    token: 0xcafe,
                    ..
                } => saw_notification = true,
                ComponentActorEvent::WorkerFinished { worker, output }
                    if worker == waiting_worker =>
                {
                    ensure!(output == b"Z", "notification worker read stale shared data");
                    saw_waiting_result = true;
                }
                ComponentActorEvent::WorkerFinished { worker, output }
                    if worker == acknowledgement_worker =>
                {
                    ensure!(
                        output == b"notification-ok",
                        "actor notification carried the wrong generation or token"
                    );
                    saw_acknowledgement = true;
                }
                ComponentActorEvent::WorkerFailed { worker, message }
                    if worker == waiting_worker || worker == acknowledgement_worker =>
                {
                    anyhow::bail!("notification worker {} failed: {message}", worker.raw());
                }
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("component notification flow timed out")??;
    Ok(())
}

async fn wait_for_dispatch(actor: &mut ComponentActor) -> Result<()> {
    loop {
        let event = next_event(actor, "waiting for dispatcher").await?;
        match event {
            ComponentActorEvent::DispatcherReturned => return Ok(()),
            ComponentActorEvent::DispatcherFailed { message } => {
                anyhow::bail!("actor dispatcher failed: {message}");
            }
            _ => {}
        }
    }
}

async fn wait_for_dispatch_and_pending_io(
    actor: &mut ComponentActor,
    expected_path: &str,
) -> Result<()> {
    let mut dispatched = false;
    let mut pending = false;
    while !dispatched || !pending {
        let event = next_event(actor, "waiting for pending worker I/O").await?;
        match event {
            ComponentActorEvent::DispatcherReturned => dispatched = true,
            ComponentActorEvent::WorkerIoPending { path }
                if path == std::path::Path::new(expected_path) =>
            {
                pending = true;
            }
            ComponentActorEvent::DispatcherFailed { message } => {
                anyhow::bail!("actor dispatcher failed: {message}");
            }
            _ => {}
        }
    }
    Ok(())
}

async fn wait_for_dispatch_failure(actor: &mut ComponentActor) -> Result<()> {
    loop {
        let event = next_event(actor, "waiting for dispatcher failure").await?;
        if let ComponentActorEvent::DispatcherFailed { message } = event {
            ensure!(
                message.contains("deadline exceeded"),
                "dispatcher failed for the wrong reason: {message}"
            );
            return Ok(());
        }
    }
}

async fn wait_for_worker_cancelled(actor: &mut ComponentActor, expected: WorkerId) -> Result<()> {
    loop {
        let event = next_event(actor, "waiting for worker cancellation").await?;
        match event {
            ComponentActorEvent::WorkerCancelled { worker } if worker == expected => return Ok(()),
            ComponentActorEvent::DispatcherFailed { message } => {
                anyhow::bail!("actor failed while cancelling worker: {message}");
            }
            _ => {}
        }
    }
}

async fn wait_for_worker(actor: &mut ComponentActor, expected: WorkerId) -> Result<Vec<u8>> {
    loop {
        let event = next_event(actor, "waiting for worker result").await?;
        match event {
            ComponentActorEvent::WorkerFinished { worker, output } if worker == expected => {
                return Ok(output);
            }
            ComponentActorEvent::WorkerFailed { worker, message } if worker == expected => {
                anyhow::bail!("worker {} failed: {message}", worker.raw());
            }
            _ => {}
        }
    }
}

async fn wait_for_worker_failed(actor: &mut ComponentActor, expected: WorkerId) -> Result<String> {
    loop {
        let event = next_event(actor, "waiting for worker failure").await?;
        match event {
            ComponentActorEvent::WorkerFailed { worker, message } if worker == expected => {
                return Ok(message);
            }
            ComponentActorEvent::DispatcherFailed { message } => {
                anyhow::bail!("actor failed while reporting worker failure: {message}");
            }
            _ => {}
        }
    }
}

async fn next_event(
    actor: &mut ComponentActor,
    operation: &'static str,
) -> Result<ComponentActorEvent> {
    timeout(Duration::from_secs(1), actor.recv_event())
        .await
        .with_context(|| format!("{operation} timed out"))?
        .context("actor event lane closed")
}
