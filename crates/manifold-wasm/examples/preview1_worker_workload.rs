use std::{env, fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use manifold_wasm::{
    Preview1ActorCapabilities, Preview1CompatActor, Preview1CompatStats, Preview1FilesystemGrant,
    Preview1WorkerConfig, Preview1WorkerError, Preview1WorkerRuntime,
};
use tokio::{io::AsyncWriteExt, time::timeout};
use wasmtime_wasi::cli::AsyncStdinStream;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let guest = PathBuf::from(
        env::args_os()
            .nth(1)
            .context("usage: preview1_worker_workload <guest.wasm>")?,
    );

    let mut runtime = Preview1WorkerRuntime::load(
        &guest,
        Preview1ActorCapabilities::default(),
        Preview1WorkerConfig::default(),
    )
    .await?;

    let (mut first_input, first_stdin) = tokio::io::duplex(1);
    let first = runtime
        .spawn_worker_with_stdin("worker_read", 0, AsyncStdinStream::new(first_stdin))
        .await?;

    wait_for_actor_at_least(&runtime, "actor_readers_entered", 1).await?;

    let ping = timeout(
        Duration::from_millis(100),
        runtime.call_actor("actor_ping", 41),
    )
    .await
    .context("actor ping stalled behind an ordinary worker Read")??;
    ensure!(ping == 42, "actor ping returned {ping}, expected 42");

    first_input.write_all(b"Z").await?;
    let value = timeout(Duration::from_secs(1), first.join())
        .await
        .context("ordinary worker Read did not resume after input")??;
    ensure!(value == u32::from(b'Z'), "worker returned {value}");
    let shared = runtime.call_actor("actor_result", 0).await?;
    ensure!(
        shared == u32::from(b'Z'),
        "actor observed shared result {shared}"
    );

    let (_spin_input, spin_stdin) = tokio::io::duplex(1);
    let spinner = runtime
        .spawn_worker_with_stdin("worker_spin", 0, AsyncStdinStream::new(spin_stdin))
        .await?;
    wait_for_actor_at_least(&runtime, "actor_spinners_entered", 1).await?;
    let ping = timeout(
        Duration::from_millis(100),
        runtime.call_actor("actor_ping", 41),
    )
    .await
    .context("CPU-running worker did not yield to its actor")??;
    ensure!(ping == 42, "actor ping returned {ping}, expected 42");
    runtime.cancel_worker(spinner.id()).await?;
    ensure!(
        matches!(spinner.join().await, Err(Preview1WorkerError::Cancelled)),
        "cancelled CPU worker delivered a late result"
    );

    let (_second_input, second_stdin) = tokio::io::duplex(1);
    let second = runtime
        .spawn_worker_with_stdin("worker_read", 0, AsyncStdinStream::new(second_stdin))
        .await?;
    wait_for_actor_at_least(&runtime, "actor_readers_entered", 2).await?;
    let after_second_instance = runtime.call_actor("actor_result", 0).await?;
    ensure!(
        after_second_instance == u32::from(b'Z'),
        "secondary instance reinitialized shared static state"
    );
    runtime.shutdown().await?;
    ensure!(
        matches!(second.join().await, Err(Preview1WorkerError::Cancelled)),
        "shutdown delivered a late worker result"
    );
    drop(runtime);

    let worker_filesystem_root = tempfile::tempdir()?;
    let worker_file_contents = b"ordinary worker filesystem round-trip\n";
    fs::write(
        worker_filesystem_root.path().join("worker-input.txt"),
        worker_file_contents,
    )?;
    let mut filesystem_runtime = Preview1WorkerRuntime::load(
        &guest,
        Preview1ActorCapabilities {
            filesystem: Some(Preview1FilesystemGrant {
                host_path: worker_filesystem_root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: true,
            }),
        },
        Preview1WorkerConfig::default(),
    )
    .await?;
    let filesystem_worker = filesystem_runtime
        .spawn_worker("worker_file_roundtrip", 0)
        .await?;
    let bytes_written = timeout(Duration::from_secs(1), filesystem_worker.join())
        .await
        .context("ordinary worker filesystem round-trip did not finish")??;
    ensure!(
        bytes_written == u32::try_from(worker_file_contents.len())?,
        "worker reported {bytes_written} filesystem bytes"
    );
    filesystem_runtime.shutdown().await?;
    drop(filesystem_runtime);
    let output = fs::read(worker_filesystem_root.path().join("worker-output.txt"))?;
    ensure!(
        output == worker_file_contents,
        "worker filesystem output did not match its input"
    );

    let capability_root = tempfile::tempdir()?;
    fs::write(
        capability_root.path().join("completion.txt"),
        b"Q-completion",
    )?;
    let capabilities = Preview1ActorCapabilities {
        filesystem: Some(Preview1FilesystemGrant {
            host_path: capability_root.path().to_path_buf(),
            guest_path: ".".to_string(),
            writable: false,
        }),
    };
    let mut compat = Preview1CompatActor::load(
        &guest,
        capabilities.clone(),
        Preview1WorkerConfig::default(),
    )
    .await?;
    let first_generation = compat.stats().generation;
    compat.try_send_message("workload".to_string(), b"start-notify".to_vec())?;
    wait_for_compat(&compat, "two workers waiting on shared signals", |stats| {
        stats.mailbox_dispatched >= 1
            && stats.workers_started >= 2
            && stats.wait_registrations == 4
            && stats.notifications == 3
            && stats.pollables == 4
    })
    .await?;
    compat.try_send_message("workload".to_string(), b"probe".to_vec())?;
    wait_for_compat(
        &compat,
        "mailbox progress while notification workers wait",
        |stats| stats.mailbox_dispatched >= 2 && stats.wait_registrations == 4,
    )
    .await?;
    compat.try_send_message("workload".to_string(), b"alternate:B".to_vec())?;
    wait_for_compat(&compat, "one selected alternate signal", |stats| {
        stats.mailbox_dispatched >= 3
            && stats.workers_finished >= 1
            && stats.wait_registrations == 2
    })
    .await?;
    compat.try_send_message("workload".to_string(), b"queue:A".to_vec())?;
    wait_for_compat(&compat, "shared queue and coalesced result", |stats| {
        stats.mailbox_dispatched >= 4
            && stats.workers_finished >= 2
            && stats.notifications_delivered == 1
            && stats.wait_registrations == 0
            && stats.pollables == 0
    })
    .await?;
    let coordinated_results = compat.call_actor("actor_coordinated_results", 0).await?;
    ensure!(
        coordinated_results == 2,
        "shared-memory workers returned {coordinated_results} coordinated results"
    );
    let coordination_generation = compat
        .call_actor("actor_coordination_generation", 0)
        .await?;
    ensure!(
        coordination_generation == 3,
        "coalesced actor notification reported generation {coordination_generation}"
    );

    // Keep normal blocking-looking WASI stdin as a compatibility adapter, but
    // do not use it as the actor/worker coordination mechanism.
    compat.try_send_message("workload".to_string(), b"start-stdin".to_vec())?;
    wait_for_compat(&compat, "ordinary worker read entered", |stats| {
        stats.mailbox_dispatched >= 5
            && stats.workers_started >= 3
            && stats.worker_reads_started >= 1
    })
    .await?;
    compat.try_send_message("workload".to_string(), b"probe".to_vec())?;
    wait_for_compat(
        &compat,
        "mailbox progress while worker read is pending",
        |stats| stats.mailbox_dispatched >= 6,
    )
    .await?;
    ensure!(
        compat.stats().workers_finished == 2,
        "ordinary worker read finished before actor progress probe"
    );
    compat.try_send_message("workload".to_string(), b"feed:Z".to_vec())?;
    wait_for_compat(&compat, "completion and worker terminal events", |stats| {
        stats.mailbox_dispatched >= 7
            && stats.completion_delivered >= 1
            && stats.workers_finished >= 3
    })
    .await?;
    let flags = compat.call_actor("actor_compat_flags", 0).await?;
    ensure!(
        flags & (1_u32 << 31) == 0 && flags & 0b1_1111 == 0b1_1111,
        "compat actor flags were {flags:#010x}"
    );
    let completion_byte = compat.call_actor("actor_compat_completion_byte", 0).await?;
    ensure!(
        completion_byte == u32::from(b'Q'),
        "compat completion returned byte {completion_byte}"
    );
    let worker_result = compat.call_actor("actor_result", 0).await?;
    ensure!(
        worker_result == u32::from(b'Z'),
        "compat worker returned {worker_result}"
    );
    compat.shutdown().await?;
    drop(compat);

    let mut replacement =
        Preview1CompatActor::load(&guest, capabilities, Preview1WorkerConfig::default()).await?;
    ensure!(
        replacement.stats().generation != first_generation,
        "compat actor reload reused generation {first_generation}"
    );
    replacement.shutdown().await?;
    drop(replacement);

    println!(
        "preview1_worker_workload=ok ordinary_read_pending=true actor_progress=true \
         worker_resumed=true shared_atomic_result=true cpu_worker_yielded=true \
         secondary_init_preserved_state=true shutdown_no_late_result=true \
         compat_mailbox=true actor_completion_io=true guest_worker_spawn=true \
         compat_actor_progress=true shared_queue=true worker_wait_any=true \
         notification_coalesced=true wait_teardown=true ordinary_read_observed=true \
         ordinary_filesystem_roundtrip=true compat_reload_generation=true"
    );
    Ok(())
}

async fn wait_for_compat(
    actor: &Preview1CompatActor,
    label: &str,
    ready: impl Fn(Preview1CompatStats) -> bool + Send + Sync,
) -> Result<()> {
    timeout(Duration::from_secs(2), async {
        loop {
            if ready(actor.stats()) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .with_context(|| format!("compat actor did not report {label}"))?;
    Ok(())
}

async fn wait_for_actor_at_least(
    runtime: &Preview1WorkerRuntime,
    export: &str,
    expected: u32,
) -> Result<()> {
    timeout(Duration::from_secs(1), async {
        loop {
            if runtime.call_actor(export, 0).await? >= expected {
                return Ok::<(), Preview1WorkerError>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .with_context(|| format!("{export} did not reach {expected}"))??;
    Ok(())
}
