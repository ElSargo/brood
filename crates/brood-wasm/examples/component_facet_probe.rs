use std::{
    env,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use brood_wasm::{
    ActorCompletionKind, ComponentActor, ComponentActorConfig, ComponentActorEvent,
    FilesystemGrant,
    facet_linker::link_denied_imports,
    facet_probe::{FacetProbeConfig, FacetProbeInput, run_facet_probe},
    snapshot_budget::{BudgetedBytes, ByteBudget, ByteBudgetLimits},
    snapshot_store::{SnapshotStore, SnapshotStoreLimits},
};
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker},
};

const MAX_MEMORY_BYTES: usize = 4 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const FUEL_PER_CALL: u64 = 25_000_000;
const SAMPLES: usize = 100;

struct EpochDriver {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl EpochDriver {
    fn start(engine: Engine) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                thread::park_timeout(Duration::from_millis(2));
                engine.increment_epoch();
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for EpochDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _joined = thread.join();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let component_path =
        PathBuf::from(env::args_os().nth(1).context(
            "usage: component_facet_probe <facet.component.wasm> <actor.component.wasm>",
        )?);
    let actor_path =
        PathBuf::from(env::args_os().nth(2).context(
            "usage: component_facet_probe <facet.component.wasm> <actor.component.wasm>",
        )?);
    let capability = tempfile::tempdir()?;
    let actor_config = ComponentActorConfig {
        filesystem: Some(FilesystemGrant {
            host_path: capability.path().into(),
            guest_path: ".".into(),
            writable: true,
        }),
        epoch_tick_interval: Duration::from_millis(1),
        epoch_yield_ticks: 1,
        ..ComponentActorConfig::default()
    };
    let mut associated = ComponentActor::load(&actor_path, actor_config.clone()).await?;
    let mut peer = ComponentActor::load(&actor_path, actor_config).await?;
    associated.try_send_message("component-facet".into(), b"spawn-spin-worker".to_vec())?;
    tokio::time::timeout(Duration::from_secs(2), wait_for_worker(&mut associated)).await??;
    associated.try_send_message("component-facet".into(), b"spin-dispatch".to_vec())?;
    tokio::time::timeout(
        Duration::from_secs(2),
        wait_for_marker(capability.path().join("dispatch-spin-entered")),
    )
    .await??;
    peer.try_send_message("component-facet".into(), b"peer-progress".to_vec())?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let generation = associated.generation();
    let worker = thread::Builder::new()
        .name("component-facet-probe".into())
        .spawn(move || {
            let outcome = run_component(&component_path, generation);
            let _receiver_closed = sender.send(outcome);
        })?;
    // This experiment admits exactly one worker and one completion. Guest
    // execution and compilation never run on the async actor runtime lane.
    let (completed, peer_progress) = tokio::join!(
        receiver,
        tokio::time::timeout(Duration::from_secs(2), wait_for_dispatch(&mut peer))
    );
    worker
        .join()
        .map_err(|_panic| anyhow::anyhow!("facet worker panicked"))?;
    completed.context("facet worker lost completion")??;
    peer_progress??;
    tokio::time::timeout(Duration::from_secs(2), peer.shutdown()).await??;
    drop(peer);
    tokio::time::timeout(Duration::from_secs(2), associated.shutdown()).await??;
    drop(associated);
    Ok(())
}

fn run_component(component_path: &Path, generation: u64) -> Result<()> {
    let mut engine_config = Config::new();
    engine_config
        .wasm_component_model(true)
        .wasm_component_model_async(true)
        .concurrency_support(true)
        .consume_fuel(true)
        .epoch_interruption(true);
    let engine = Engine::new(&engine_config)
        .map_err(|error| anyhow::anyhow!("create component facet engine: {error}"))?;
    let component = Component::from_file(&engine, component_path).map_err(|error| {
        anyhow::anyhow!("load component facet {}: {error}", component_path.display())
    })?;
    // Trusted plugins may reuse the actor artifact. Its host operations are
    // linked to correctly typed traps in this independent facet instance.
    // Dynamic argument lifting before denial is an accepted limitation.
    let mut linker = Linker::new(&engine);
    link_denied_imports(&mut linker, &component)
        .map_err(|error| anyhow::anyhow!("link facet denial stubs: {error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let _epochs = EpochDriver::start(engine.clone());
    let limits = StoreLimitsBuilder::new()
        .memory_size(MAX_MEMORY_BYTES)
        // The canonical adapter contributes its own bounded core instance and
        // table in addition to the guest module.
        .instances(4)
        .memories(1)
        .tables(2)
        .table_elements(128)
        .build();
    let mut store = Store::new(&engine, limits);
    store.limiter(|limits: &mut StoreLimits| limits);
    store
        .set_fuel(FUEL_PER_CALL)
        .map_err(|error| anyhow::anyhow!("set component instantiation fuel: {error}"))?;
    store.set_epoch_deadline(50);
    store.epoch_deadline_trap();
    let instance = runtime
        .block_on(linker.instantiate_async(&mut store, &component))
        .map_err(|error| anyhow::anyhow!("instantiate component facet: {error}"))?;
    let render = instance
        .get_typed_func::<(&[u8],), (Vec<u8>,)>(&mut store, "render")
        .map_err(|error| anyhow::anyhow!("load typed component render export: {error}"))?;

    let snapshots = SnapshotStore::new(
        generation,
        SnapshotStoreLimits {
            max_live_bytes: 4 * MAX_INPUT_BYTES,
            max_live_allocations: 4,
            max_live_roots: 4,
            max_snapshot_bytes: MAX_INPUT_BYTES,
            max_chunks_per_snapshot: 1,
        },
    );
    let transfers = ByteBudget::new(
        generation,
        ByteBudgetLimits {
            // One staging buffer, one input transfer and one worst-case owned
            // canonical result. Guest memory has its own independent store bound.
            max_live_bytes: 2 * MAX_INPUT_BYTES + MAX_MEMORY_BYTES,
            max_live_allocations: 3,
            max_buffer_bytes: MAX_MEMORY_BYTES,
        },
    );

    let mut inputs = vec![("session-list-128".to_owned(), session_list()?)];
    for rows in [24_usize, 80, 240, 1_000] {
        inputs.push((format!("terminal-rows-{rows}"), terminal_rows(rows)?));
    }
    inputs.push(("transfer-1mib".to_owned(), vec![0x5a; MAX_INPUT_BYTES]));
    let fixture_bytes: usize = inputs.iter().map(|(_, bytes)| bytes.capacity()).sum();
    println!("trusted_fixture_capacity_bytes={fixture_bytes} samples_per_shape={SAMPLES}");
    let mut revision = 0;
    for (label, input) in &inputs {
        let mut timings = Vec::with_capacity(SAMPLES);
        for _sample in 0..SAMPLES {
            ensure!(
                input.len() <= MAX_INPUT_BYTES,
                "{label} exceeds component input limit"
            );
            let staging = BudgetedBytes::copy_from_slice(&transfers, input)?;
            revision += 1;
            snapshots.publish_copy_of(revision, staging.bytes()?)?;
            let snapshot = snapshots
                .acquire_latest()?
                .context("published view missing")?;
            let transfer = BudgetedBytes::copy_snapshot(&transfers, snapshot)?;
            store
                .set_fuel(FUEL_PER_CALL)
                .map_err(|error| anyhow::anyhow!("reset component facet fuel: {error}"))?;
            store.set_epoch_deadline(50);
            let started = Instant::now();
            // Vec lifting copies before Wasmtime's automatic guest post-return.
            // Borrowed WasmList bytes can already be freed/overwritten at return.
            // Flat list<u8> ranges are checked against the single bounded memory
            // before lifting; reserve that full bound before the runtime can copy.
            let output = BudgetedBytes::capture_bounded(&transfers, MAX_MEMORY_BYTES, || {
                runtime
                    .block_on(render.call_async(
                        &mut store,
                        (transfer.bytes().map_err(|error| error.to_string())?,),
                    ))
                    .map(|(bytes,)| bytes)
                    .map_err(|error| format!("call component facet for {label}: {error}"))
            })
            .map_err(|error| anyhow::anyhow!("capture canonical result: {error}"))?;
            let elapsed = started.elapsed();
            ensure!(
                output.len() <= MAX_OUTPUT_BYTES,
                "{label} output exceeds component output limit"
            );
            ensure!(
                output.len() == 16,
                "{label} returned malformed probe output"
            );
            let (checksum_bytes, length_bytes) = output.bytes()?.split_at(8);
            let checksum = u64::from_le_bytes(checksum_bytes.try_into()?);
            let returned_length = u64::from_le_bytes(length_bytes.try_into()?);
            let expected_checksum = input
                .iter()
                .fold(0_u64, |sum, byte| sum.wrapping_add(u64::from(*byte)));
            ensure!(
                checksum == expected_checksum && returned_length == u64::try_from(input.len())?,
                "{label} returned the wrong checksum or input length"
            );
            ensure!(
                transfer.is_authoritative() && output.is_authoritative(),
                "generation invalidated before result acceptance"
            );
            timings.push(elapsed);
            drop(output);
            drop(transfer);
            drop(staging);
            snapshots.reclaim_retired();
            ensure!(
                transfers.retention().live_bytes == 0,
                "transfer reservations leaked"
            );
        }
        timings.sort_unstable();
        println!(
            "{label} input={}B output=16B canonical_call p50={:?} p95={:?} p99={:?} max={:?}",
            input.len(),
            timings.get(49).context("missing median sample")?,
            timings.get(94).context("missing p95 sample")?,
            timings.get(98).context("missing p99 sample")?,
            timings.last().context("missing maximum sample")?
        );
    }
    // Move fixtures into the core comparison after canonical measurements;
    // do not retain a second cloned fixture corpus alongside pipeline buffers.
    let core_inputs = inputs
        .into_iter()
        .map(|(label, bytes)| FacetProbeInput { label, bytes })
        .collect();
    let core = run_facet_probe(
        include_bytes!("fixtures/facet-probe.wat"),
        core_inputs,
        FacetProbeConfig::default(),
    )?;
    for timing in core.timings {
        println!(
            "{} core_input={}B core_copy_in={:?} core_call={:?} core_copy_out={:?}",
            timing.label, timing.input_bytes, timing.copy_in, timing.guest_call, timing.copy_out
        );
    }
    store
        .set_fuel(FUEL_PER_CALL)
        .map_err(|error| anyhow::anyhow!("reset oversized-output fuel: {error}"))?;
    store.set_epoch_deadline(50);
    let oversized = BudgetedBytes::capture_bounded(&transfers, MAX_MEMORY_BYTES, || {
        runtime
            .block_on(render.call_async(&mut store, (b"oversized-output-probe".as_slice(),)))
            .map(|(bytes,)| bytes)
            .map_err(|error| error.to_string())
    });
    ensure!(
        oversized.is_err(),
        "component canonical lift admitted output beyond the guest memory bound"
    );
    println!("oversized component output trapped before host acceptance");
    ensure!(
        transfers.retention().live_bytes == 0,
        "failed call leaked reservation"
    );
    snapshots.invalidate();
    snapshots.reclaim_retired();
    transfers.invalidate();
    ensure!(
        snapshots.retention().live_bytes == 0,
        "snapshot teardown leaked payloads"
    );
    Ok(())
}

async fn wait_for_worker(actor: &mut ComponentActor) -> Result<()> {
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
            ) => anyhow::bail!(message),
            Some(ComponentActorEvent::Stopped) | None => anyhow::bail!("associated actor stopped"),
            Some(_) => {}
        }
    }
}

async fn wait_for_marker(path: PathBuf) -> Result<()> {
    loop {
        match std::fs::read(&path) {
            Ok(bytes) if bytes == b"yes" => return Ok(()),
            Ok(_) => anyhow::bail!("actor checkpoint contents differ"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn wait_for_dispatch(actor: &mut ComponentActor) -> Result<()> {
    loop {
        match actor.recv_event().await {
            Some(ComponentActorEvent::DispatcherReturned) => return Ok(()),
            Some(ComponentActorEvent::DispatcherFailed { message }) => anyhow::bail!(message),
            Some(ComponentActorEvent::Stopped) | None => anyhow::bail!("peer actor stopped"),
            Some(_) => {}
        }
    }
}

fn session_list() -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::json!({
        "revision": 41,
        "sessions": (0..128).map(|id| serde_json::json!({
            "id": format!("session-{id}"), "title": format!("project {id}"), "active": id == 17
        })).collect::<Vec<_>>()
    }))?)
}

fn terminal_rows(rows: usize) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::json!({
        "revision": 1000 + rows,
        "rows": (0..rows).map(|row| format!("{row:04}: component facet row with changing terminal content λ界"))
            .collect::<Vec<_>>()
    }))?)
}
