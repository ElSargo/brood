use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use manifold_wasm::{
    ActorCompletion, ActorCompletionKind, ComponentActor, ComponentActorConfig,
    ComponentActorEvent, CoordinationConfig, CoordinationHub, FilesystemGrant, NotificationError,
    OperationId, PollError, Preview1ActorCapabilities, Preview1FilesystemGrant,
    Preview1WorkerConfig, Preview1WorkerError, Preview1WorkerRuntime, WaitMode, WorkerId,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::AsyncWriteExt,
    time::{sleep, timeout},
};
use wasmtime_wasi::cli::AsyncStdinStream;

const MESSAGE_MAGIC: &[u8; 8] = b"RUBEIO01";
const COMPONENT_WORKER_BASE: u64 = 1_u64 << 40;
const QUICK_ACTORS: usize = 32;
const QUICK_WORKERS: usize = 4;
const QUICK_STEPS: u64 = 1_536;
const STRESS_ACTORS: usize = 48;
const STRESS_WORKERS: usize = 8;
const STRESS_STEPS: u64 = 100_000;
const SOAK_ACTORS: usize = 48;
const SOAK_WORKERS: usize = 12;
const SOAK_STEPS: u64 = 8_192;
const SOAK_SECONDS: u64 = 3_600;
const TRACE_RECORD_LIMIT: u64 = 20_000;
const PLAN_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Profile {
    Quick,
    Stress,
    Soak,
}

impl Profile {
    const fn name(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Stress => "stress",
            Self::Soak => "soak",
        }
    }
}

#[derive(Debug)]
struct Settings {
    executable: PathBuf,
    component: PathBuf,
    preview: PathBuf,
    profile: Profile,
    seed: u64,
    out_dir: PathBuf,
    actors: usize,
    workers_per_actor: usize,
    steps: u64,
    epochs: Option<u64>,
    duration: Option<Duration>,
}

struct Trace {
    writer: BufWriter<File>,
    written: u64,
    omitted: u64,
}

impl Trace {
    fn create(path: &Path) -> Result<Self> {
        let file = File::create(path)
            .with_context(|| format!("create trace artifact {}", path.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
            written: 0,
            omitted: 0,
        })
    }

    fn record(&mut self, value: &Value) -> Result<()> {
        if self.written >= TRACE_RECORD_LIMIT {
            self.omitted = self.omitted.saturating_add(1);
            return Ok(());
        }
        serde_json::to_writer(&mut self.writer, value).context("encode trace record")?;
        self.writer
            .write_all(b"\n")
            .context("append trace record")?;
        self.written = self.written.saturating_add(1);
        Ok(())
    }

    fn finish(mut self) -> Result<(u64, u64)> {
        self.writer.flush().context("flush trace artifact")?;
        Ok((self.written, self.omitted))
    }
}

struct ComponentNode {
    slot: usize,
    root: TempDir,
    actor: ComponentActor,
    messages: u64,
    dropped: u64,
}

struct PreviewNode {
    slot: usize,
    root: TempDir,
    actor: Preview1WorkerRuntime,
    messages: u64,
    rube_receipts: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Backend {
    Component,
    Preview1,
}

impl Backend {
    const fn name(self) -> &'static str {
        match self {
            Self::Component => "component",
            Self::Preview1 => "preview1",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TransitionPlan {
    id: u64,
    epoch: u64,
    actor_slot: usize,
    backend: Backend,
    generation: u64,
    input: u64,
    output: u64,
}

#[derive(Clone, Copy, Debug)]
struct ExpectedReceipt {
    actor_slot: usize,
    backend: Backend,
    generation: u64,
    output: u64,
}

#[derive(Default)]
struct ReceiptLedger {
    expected: BTreeMap<u64, ExpectedReceipt>,
    component_workers: BTreeMap<WorkerId, u64>,
    planned: u64,
    received: u64,
}

impl ReceiptLedger {
    fn begin(&mut self, plan: TransitionPlan) -> Result<()> {
        let expectation = ExpectedReceipt {
            actor_slot: plan.actor_slot,
            backend: plan.backend,
            generation: plan.generation,
            output: plan.output,
        };
        ensure!(
            self.expected.insert(plan.id, expectation).is_none(),
            "transition {} was planned more than once",
            plan.id
        );
        self.planned = self
            .planned
            .checked_add(1)
            .context("receipt plan counter exhausted")?;
        if plan.backend == Backend::Component {
            let worker = component_worker_for_transition(plan.id);
            ensure!(
                self.component_workers.insert(worker, plan.id).is_none(),
                "Component worker {} was assigned to multiple transitions",
                worker.raw()
            );
        }
        Ok(())
    }

    fn finish_component(
        &mut self,
        worker: WorkerId,
        actor_slot: usize,
        generation: u64,
        output: &[u8],
    ) -> Result<u64> {
        let transition = self
            .component_workers
            .remove(&worker)
            .or_else(|| worker.raw().checked_sub(COMPONENT_WORKER_BASE))
            .with_context(|| {
                format!(
                    "stale or unordered Component receipt from unknown worker {}",
                    worker.raw()
                )
            })?;
        let output: [u8; 8] = output.try_into().with_context(|| {
            format!(
                "Component transition {transition} returned {} bytes instead of one u64 receipt",
                output.len()
            )
        })?;
        self.finish(
            transition,
            actor_slot,
            Backend::Component,
            generation,
            u64::from_le_bytes(output),
        )?;
        Ok(transition)
    }

    fn finish_preview(
        &mut self,
        transition: u64,
        actor_slot: usize,
        generation: u64,
        output: u64,
    ) -> Result<()> {
        self.finish(
            transition,
            actor_slot,
            Backend::Preview1,
            generation,
            output,
        )
    }

    fn finish(
        &mut self,
        transition: u64,
        actor_slot: usize,
        backend: Backend,
        generation: u64,
        output: u64,
    ) -> Result<()> {
        let expected = self.expected.remove(&transition).with_context(|| {
            format!("duplicate, stale, or unordered receipt for transition {transition}")
        })?;
        ensure!(
            expected.actor_slot == actor_slot
                && expected.backend == backend
                && expected.generation == generation,
            "stale or unordered transition {transition} receipt: expected {:?}, got backend={} slot={} generation={}",
            expected,
            backend.name(),
            actor_slot,
            generation
        );
        ensure!(
            expected.output == output,
            "transition {transition} checksum mismatch: expected {:016x}, got {output:016x}",
            expected.output
        );
        self.received = self
            .received
            .checked_add(1)
            .context("receipt counter exhausted")?;
        Ok(())
    }

    fn assert_complete(&self) -> Result<()> {
        ensure!(
            self.expected.is_empty(),
            "lost transition receipts remain pending: {:?}",
            self.expected.keys().collect::<Vec<_>>()
        );
        ensure!(
            self.component_workers.is_empty(),
            "Component workers remain assigned to transitions: {:?}",
            self.component_workers
        );
        ensure!(
            self.received == self.planned,
            "receipt ledger mismatch: planned {}, received {}",
            self.planned,
            self.received
        );
        Ok(())
    }

    const fn received(&self) -> u64 {
        self.received
    }
}

struct WorkloadOutcome {
    transition: u64,
    chain: u64,
    epoch: u64,
    barrier_messages: u64,
    pressure_accepted: u64,
    pressure_dropped: u64,
    pressure: MailboxPressureOutcome,
    replacement_receipts: u64,
    total_messages: u64,
    terminal_receipts: u64,
}

struct ReplacementOutcome {
    messages: u64,
    receipts: u64,
}

#[derive(Clone, Copy, Debug)]
struct MailboxPressureOutcome {
    component_accepted: u64,
    component_rejected: u64,
    preview_accepted: u64,
    preview_rejected: u64,
}

impl MailboxPressureOutcome {
    const fn accepted(self) -> u64 {
        self.component_accepted
            .saturating_add(self.preview_accepted)
    }

    const fn rejected(self) -> u64 {
        self.component_rejected
            .saturating_add(self.preview_rejected)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ResourceTotals {
    notifications: usize,
    pollables: usize,
    wait_registrations: usize,
    pending_actor_events: usize,
    completion_outstanding: usize,
    preview_workers_active: usize,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let settings = Settings::parse()?;
    fs::create_dir_all(&settings.out_dir)
        .with_context(|| format!("create output directory {}", settings.out_dir.display()))?;
    let failure_path = settings.out_dir.join("failure.json");
    let summary_path = settings.out_dir.join("summary.json");
    for stale in [&failure_path, &summary_path] {
        if stale.exists() {
            fs::remove_file(stale)
                .with_context(|| format!("remove stale result artifact {}", stale.display()))?;
        }
    }
    let result = run(&settings).await;
    if let Err(error) = &result {
        eprintln!(
            "io_rube_goldberg=failed profile={} seed={} error={error:#}",
            settings.profile.name(),
            settings.seed
        );
        eprintln!("io_rube_replay={}", settings.replay_command());
        let failure = json!({
            "schema": "manifold.io-rube-goldberg.failure.v1",
            "profile": settings.profile.name(),
            "seed": settings.seed,
            "error": format!("{error:#}"),
            "replay": settings.replay_command(),
        });
        let encoded = serde_json::to_vec_pretty(&failure).context("encode failure artifact")?;
        fs::write(&failure_path, encoded)
            .with_context(|| format!("write failure artifact {}", failure_path.display()))?;
    } else if failure_path.exists() {
        fs::remove_file(&failure_path)
            .with_context(|| format!("remove stale failure artifact {}", failure_path.display()))?;
    }
    result
}

#[allow(
    clippy::significant_drop_tightening,
    reason = "the actor vectors intentionally own live runtimes for the whole workload"
)]
async fn run(settings: &Settings) -> Result<()> {
    let started = Instant::now();
    let resource_path = settings.out_dir.join("resource-samples.jsonl");
    if resource_path.exists() {
        fs::remove_file(&resource_path).with_context(|| {
            format!("remove stale resource samples {}", resource_path.display())
        })?;
    }
    let mut trace = Trace::create(&settings.out_dir.join("trace.jsonl"))?;
    trace.record(&json!({
        "kind": "header",
        "schema": "manifold.io-rube-goldberg.trace.v1",
        "plan_version": PLAN_VERSION,
        "profile": settings.profile.name(),
        "seed": settings.seed,
        "actors": settings.actors,
        "workers_per_actor": settings.workers_per_actor,
        "steps_per_epoch": settings.steps,
        "epochs": settings.epochs,
        "duration_seconds": settings.duration.map(|duration| duration.as_secs()),
        "out_of_scope": ["subprocess", "pty", "socket", "generic-timer-pollable"],
        "replay": settings.replay_command(),
    }))?;
    write_manifest(settings)?;

    let component_count = settings.actors.div_ceil(2);
    let preview_count = settings.actors / 2;
    let mut components = Vec::with_capacity(component_count);
    let mut previews = Vec::with_capacity(preview_count);
    for index in 0..component_count {
        let Some(slot) = index.checked_mul(2) else {
            let shutdown = shutdown_all(&mut components, &mut previews).await;
            return combine_workload_and_shutdown(
                Err(anyhow::anyhow!("component slot overflow")),
                shutdown,
            );
        };
        match load_component(settings, slot).await {
            Ok(component) => components.push(component),
            Err(error) => {
                let shutdown = shutdown_all(&mut components, &mut previews).await;
                return combine_workload_and_shutdown(Err(error), shutdown);
            }
        }
    }
    for index in 0..preview_count {
        let Some(slot) = index.checked_mul(2).and_then(|slot| slot.checked_add(1)) else {
            let shutdown = shutdown_all(&mut components, &mut previews).await;
            return combine_workload_and_shutdown(
                Err(anyhow::anyhow!("Preview 1 slot overflow")),
                shutdown,
            );
        };
        match load_preview(settings, slot).await {
            Ok(preview) => previews.push(preview),
            Err(error) => {
                let shutdown = shutdown_all(&mut components, &mut previews).await;
                return combine_workload_and_shutdown(Err(error), shutdown);
            }
        }
    }
    let workload_result: Result<WorkloadOutcome> = async {
        let mut ledger = ReceiptLedger::default();
    trace.record(&json!({
        "kind": "barrier",
        "name": "actors-loaded",
        "component": components.len(),
        "preview1": previews.len(),
    }))?;

    run_coordination_barrier(settings.seed).await?;
    trace.record(&json!({
        "kind": "barrier",
        "name": "coordination-hub-adversarial",
        "covered": [
            "signal-before-watch",
            "watch-before-signal",
            "actor-unwatch",
            "try-poll",
            "biased-select",
            "fair-select",
            "relative-timeout",
            "pollable-drop-during-wait",
            "notification-drop-invalidation",
            "empty-duplicate-over-limit-wait-sets",
            "foreign-handle-rejection",
        ],
    }))?;

    boot_component_workers(&mut components, settings.workers_per_actor).await?;
    boot_preview_workers(&mut previews, settings.workers_per_actor).await?;
    trace.record(&json!({
        "kind": "barrier",
        "name": "workers-booted",
        "worker_lifecycles": settings.actors.saturating_mul(settings.workers_per_actor),
        "surfaces": [
            "component-worker-async-file-read",
            "preview1-worker-standard-wasi-open-read-write",
            "shared-actor-address-space",
        ],
    }))?;

    run_component_barriers(&mut components).await?;
    run_preview_barriers(&mut previews).await?;
    let barrier_messages = components
        .iter()
        .map(|node| node.messages)
        .chain(previews.iter().map(|node| node.messages))
        .sum::<u64>();
    write_resource_samples(
        settings,
        "after-adversarial-barriers",
        &components,
        &previews,
    )?;
    trace.record(&json!({
        "kind": "barrier",
        "name": "adversarial-io-surfaces",
        "covered": [
            "completion-read-write-cancel-failure",
            "notification-watch-coalesce",
            "worker-fair-select",
            "worker-biased-select-cancellation",
            "ordinary-stdin",
            "worker-cpu-yield-cancellation",
        ],
    }))?;

    // Profile duration bounds measure the repetitive workload, not actor
    // loading or the one-time adversarial setup above.
    let workload_started = Instant::now();
    let mut transition = 0_u64;
    let mut chain = settings.seed;
    let mut epoch = 0_u64;
    loop {
        if settings.epochs.is_some_and(|maximum| epoch >= maximum)
            || settings
                .duration
                .is_some_and(|duration| workload_started.elapsed() >= duration)
        {
            break;
        }
        let epoch_start = transition;
        let epoch_end = transition
            .checked_add(settings.steps)
            .context("epoch transition bound overflowed")?;
        while transition < epoch_end {
            if settings
                .duration
                .is_some_and(|duration| workload_started.elapsed() >= duration)
            {
                break;
            }
            let remaining = epoch_end.saturating_sub(transition);
            let wave_len = usize::try_from(remaining)
                .unwrap_or(usize::MAX)
                .min(settings.actors);
            let mut wave = Vec::with_capacity(wave_len);

            // Admit one transition per actor before collecting any receipt.
            // The expected chain is deterministic, but runtime completion
            // order is free to vary across the actor Stores.
            for _ in 0..wave_len {
                if settings
                    .duration
                    .is_some_and(|duration| workload_started.elapsed() >= duration)
                {
                    break;
                }
                let slot_u64 = transition
                    % u64::try_from(settings.actors).context("actor count does not fit in u64")?;
                let slot =
                    usize::try_from(slot_u64).context("actor slot does not fit in usize")?;
                let index = slot / 2;
                let backend = if slot % 2 == 0 {
                    Backend::Component
                } else {
                    Backend::Preview1
                };
                let generation = match backend {
                    Backend::Component => components
                        .get(index)
                        .context("planned Component actor slot is missing")?
                        .actor
                        .generation(),
                    Backend::Preview1 => previews
                        .get(index)
                        .context("planned Preview 1 actor slot is missing")?
                        .actor
                        .stats()
                        .generation,
                };
                let output = rube_mix(chain, transition, generation);
                let plan = TransitionPlan {
                    id: transition,
                    epoch,
                    actor_slot: slot,
                    backend,
                    generation,
                    input: chain,
                    output,
                };
                ledger.begin(plan)?;
                let message = encode_message(transition, chain, generation, output);
                match backend {
                    Backend::Component => {
                        let node = components
                            .get_mut(index)
                            .context("Component actor disappeared during transition admission")?;
                        node.actor
                            .try_send_message("io-rube-goldberg".to_string(), message)?;
                        node.messages = node.messages.saturating_add(1);
                    }
                    Backend::Preview1 => {
                        let node = previews
                            .get_mut(index)
                            .context("Preview 1 actor disappeared during transition admission")?;
                        assert_preview_receipt_count(node).await?;
                        node.actor
                            .try_send_message("io-rube-goldberg".to_string(), message)?;
                        node.messages = node.messages.saturating_add(1);
                    }
                }
                wave.push(plan);
                chain = output;
                transition = transition
                    .checked_add(1)
                    .context("transition ID exhausted")?;
            }

            if wave.is_empty() {
                break;
            }
            for plan in wave {
                let index = plan.actor_slot / 2;
                match plan.backend {
                    Backend::Component => {
                        let node = components
                            .get_mut(index)
                            .context("Component actor disappeared during receipt collection")?;
                        wait_component_receipt(node, &mut ledger, plan)
                            .await
                            .with_context(|| {
                                format!(
                                    "transition {} failed in Component slot {}",
                                    plan.id, plan.actor_slot
                                )
                            })?;
                    }
                    Backend::Preview1 => {
                        let node = previews
                            .get_mut(index)
                            .context("Preview 1 actor disappeared during receipt collection")?;
                        wait_preview_receipt(node, &mut ledger, plan)
                            .await
                            .with_context(|| {
                                format!(
                                    "transition {} failed in Preview 1 slot {}",
                                    plan.id, plan.actor_slot
                                )
                            })?;
                    }
                }
                trace.record(&json!({
                    "kind": "transition",
                    "id": plan.id,
                    "epoch": plan.epoch,
                    "actor_slot": plan.actor_slot,
                    "backend": plan.backend.name(),
                    "generation": plan.generation,
                    "input": format!("{:016x}", plan.input),
                    "output": format!("{:016x}", plan.output),
                    "outcome": "exactly-once",
                }))?;
            }
        }
        trace.record(&json!({
            "kind": "barrier",
            "name": "epoch-complete",
            "epoch": epoch,
            "first_transition": epoch_start,
            "last_transition_exclusive": transition,
            "chain": format!("{chain:016x}"),
        }))?;
        write_resource_samples(
            settings,
            &format!("epoch-{epoch}-complete"),
            &components,
            &previews,
        )?;
        epoch = epoch.checked_add(1).context("epoch counter exhausted")?;
    }

    let actual_messages = components
        .iter()
        .map(|node| node.messages)
        .chain(previews.iter().map(|node| node.messages))
        .sum::<u64>();
    let pressure = run_mailbox_backpressure(&mut components, &mut previews).await?;
    let pressure_accepted = pressure.accepted();
    let pressure_dropped = pressure.rejected();
    let actual_messages = actual_messages.saturating_add(pressure_accepted);
    ensure!(
        actual_messages
            .saturating_sub(barrier_messages)
            .saturating_sub(pressure_accepted)
            == transition,
        "message accounting mismatch: expected {transition} high-volume messages after {barrier_messages} barrier and {pressure_accepted} pressure messages, observed {actual_messages} total"
    );
    trace.record(&json!({
        "kind": "barrier",
        "name": "bounded-mailbox-backpressure",
        "accepted": pressure_accepted,
        "rejected": pressure_dropped,
        "component": {
            "accepted": pressure.component_accepted,
            "rejected": pressure.component_rejected,
        },
        "preview1": {
            "accepted": pressure.preview_accepted,
            "rejected": pressure.preview_rejected,
        },
    }))?;
    write_resource_samples(
        settings,
        "after-mailbox-backpressure",
        &components,
        &previews,
    )?;

    let replacement = run_replacement_barrier(
        settings,
        &mut components,
        &mut previews,
        &mut ledger,
    )
    .await?;
    verify_no_extra_component_receipts(&mut components, &mut ledger).await?;
    for preview in &previews {
        wait_preview_rube_quiescent(preview).await?;
    }
    ledger.assert_complete()?;
    let total_messages = actual_messages.saturating_add(replacement.messages);
    let total_receipts = ledger.received();
    write_resource_samples(settings, "after-hot-replacement", &components, &previews)?;
    trace.record(&json!({
        "kind": "barrier",
        "name": "hot-replacement-and-stale-generation",
        "replacement_receipts": replacement.receipts,
        "old_completion_cancel_rejected": true,
        "old_coordination_resources": 0,
    }))?;
    Ok(WorkloadOutcome {
        transition,
        chain,
        epoch,
        barrier_messages,
        pressure_accepted,
        pressure_dropped,
        pressure,
        replacement_receipts: replacement.receipts,
        total_messages,
        terminal_receipts: total_receipts,
    })
    }
    .await;

    let shutdown_result = shutdown_all(&mut components, &mut previews).await;
    let outcome = combine_workload_and_shutdown(workload_result, shutdown_result)?;
    let resources = resource_totals(&components, &previews);
    ensure!(
        resources.notifications == 0
            && resources.pollables == 0
            && resources.wait_registrations == 0
            && resources.pending_actor_events == 0
            && resources.completion_outstanding == 0
            && resources.preview_workers_active == 0,
        "measured resources remained after shutdown: {resources:?}"
    );
    write_resource_samples(settings, "after-shutdown", &components, &previews)?;
    trace.record(&json!({
        "kind": "barrier",
        "name": "zero-resources-after-shutdown",
        "notifications": resources.notifications,
        "pollables": resources.pollables,
        "wait_registrations": resources.wait_registrations,
        "pending_actor_events": resources.pending_actor_events,
        "completion_outstanding": resources.completion_outstanding,
        "preview_workers_active": resources.preview_workers_active,
    }))?;
    let (trace_records, trace_omitted) = trace.finish()?;

    let WorkloadOutcome {
        transition,
        chain,
        epoch,
        barrier_messages,
        pressure_accepted,
        pressure_dropped,
        pressure,
        replacement_receipts,
        total_messages,
        terminal_receipts,
    } = outcome;

    let summary = json!({
        "schema": "manifold.io-rube-goldberg.summary.v1",
        "plan_version": PLAN_VERSION,
        "status": "ok",
        "profile": settings.profile.name(),
        "seed": settings.seed,
        "actors": settings.actors,
        "workers_per_actor": settings.workers_per_actor,
        "epochs_completed": epoch,
        "transitions_planned_per_epoch": settings.steps,
        "transitions_completed": transition,
        "messages_admitted": total_messages,
        "barrier_messages": barrier_messages,
        "mailbox_pressure_accepted": pressure_accepted,
        "mailbox_pressure_rejected": pressure_dropped,
        "mailbox_pressure_by_backend": {
            "component": {
                "accepted": pressure.component_accepted,
                "rejected": pressure.component_rejected,
            },
            "preview1": {
                "accepted": pressure.preview_accepted,
                "rejected": pressure.preview_rejected,
            },
        },
        "replacement_receipts": replacement_receipts,
        "high_volume_terminal_receipts": transition,
        "terminal_receipts": terminal_receipts,
        "duplicates": 0,
        "lost": 0,
        "stale_deliveries": 0,
        "stale_operations_rejected": 1,
        "final_chain": format!("{chain:016x}"),
        "trace_records": trace_records,
        "trace_records_omitted": trace_omitted,
        "elapsed_millis": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "coordination_after_shutdown": {
            "notifications": resources.notifications,
            "pollables": resources.pollables,
            "wait_registrations": resources.wait_registrations,
            "pending_actor_events": resources.pending_actor_events,
            "completion_outstanding": resources.completion_outstanding,
            "preview_workers_active": resources.preview_workers_active,
        },
        "covered": [
            "actor-global-mailbox",
            "bounded-mailbox-backpressure",
            "actor-completion-read-write-cancel-failure",
            "component-stackless-workers",
            "component-worker-file-read",
            "preview1-workers",
            "preview1-standard-wasi-filesystem",
            "preview1-standard-wasi-stdin",
            "shared-memory-atomic-receipt",
            "notification-watch-signal-coalescing",
            "worker-wait-any-biased-and-fair",
            "wait-timeout-and-resource-invalidation",
            "worker-cpu-yield-and-cancellation",
            "hot-replacement-and-stale-generation-rejection",
            "worker-lifecycle",
            "structured-shutdown",
        ],
        "out_of_scope": ["subprocess", "pty", "socket", "generic-timer-pollable"],
        "replay": settings.replay_command(),
    });
    let summary_path = settings.out_dir.join("summary.json");
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)
        .with_context(|| format!("write summary artifact {}", summary_path.display()))?;
    println!(
        "io_rube_goldberg=ok profile={} actors={} workers_per_actor={} transitions={} chain={chain:016x} out_dir={}",
        settings.profile.name(),
        settings.actors,
        settings.workers_per_actor,
        transition,
        settings.out_dir.display()
    );
    Ok(())
}

impl Settings {
    fn parse() -> Result<Self> {
        let mut arguments = env::args_os().skip(1);
        let executable = env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("manifold-wasm-io-rube-goldberg-host"));
        let component = PathBuf::from(arguments.next().context(
            "usage: io_rube_goldberg <component> <preview1> [--profile quick|stress|soak] [options]",
        )?);
        let preview = PathBuf::from(arguments.next().context("missing Preview 1 guest path")?);
        let mut profile = Profile::Quick;
        let mut seed = 0x4d41_4e49_464f_4c44_u64;
        let mut out_dir = None;
        let mut actors = None;
        let mut workers = None;
        let mut steps = None;
        let mut epochs = None;
        let mut duration_seconds = None;
        while let Some(flag) = arguments.next() {
            let flag = flag.to_string_lossy();
            let value = arguments
                .next()
                .with_context(|| format!("{flag} requires a value"))?;
            let value = value.to_string_lossy();
            match flag.as_ref() {
                "--profile" => {
                    profile = match value.as_ref() {
                        "quick" => Profile::Quick,
                        "stress" => Profile::Stress,
                        "soak" => Profile::Soak,
                        _ => bail!("unknown profile {value:?}; expected quick, stress, or soak"),
                    };
                }
                "--seed" => seed = parse_u64(&value).context("parse --seed")?,
                "--out-dir" => out_dir = Some(PathBuf::from(value.as_ref())),
                "--actors" => actors = Some(parse_usize(&value, "--actors")?),
                "--workers-per-actor" => {
                    workers = Some(parse_usize(&value, "--workers-per-actor")?);
                }
                "--steps" => steps = Some(parse_u64(&value).context("parse --steps")?),
                "--epochs" => epochs = Some(parse_u64(&value).context("parse --epochs")?),
                "--duration-seconds" => {
                    duration_seconds = Some(parse_u64(&value).context("parse --duration-seconds")?);
                }
                _ => bail!("unknown option {flag:?}"),
            }
        }
        let defaults = match profile {
            Profile::Quick => (QUICK_ACTORS, QUICK_WORKERS, QUICK_STEPS, Some(1), None),
            Profile::Stress => (STRESS_ACTORS, STRESS_WORKERS, STRESS_STEPS, Some(1), None),
            Profile::Soak => (
                SOAK_ACTORS,
                SOAK_WORKERS,
                SOAK_STEPS,
                None,
                Some(SOAK_SECONDS),
            ),
        };
        let actors = actors.unwrap_or(defaults.0);
        let workers_per_actor = workers.unwrap_or(defaults.1);
        let steps = steps.unwrap_or(defaults.2);
        let explicit_epochs = epochs;
        let epochs = explicit_epochs.or(defaults.3);
        let duration = duration_seconds
            .or_else(|| {
                (profile == Profile::Soak && explicit_epochs.is_none())
                    .then_some(defaults.4)
                    .flatten()
            })
            .map(Duration::from_secs);
        ensure!(actors >= 2, "at least two actors are required");
        ensure!(workers_per_actor > 0, "workers per actor must be non-zero");
        ensure!(steps > 0, "steps must be non-zero");
        ensure!(epochs != Some(0), "epochs must be non-zero");
        ensure!(
            duration != Some(Duration::ZERO),
            "duration must be non-zero"
        );
        let out_dir = out_dir.unwrap_or_else(|| {
            PathBuf::from("target/io-rube-goldberg").join(format!("{}-{seed:016x}", profile.name()))
        });
        Ok(Self {
            executable,
            component,
            preview,
            profile,
            seed,
            out_dir,
            actors,
            workers_per_actor,
            steps,
            epochs,
            duration,
        })
    }

    fn replay_command(&self) -> String {
        format!(
            "{} {} {} --profile {} --seed {} --out-dir {} --actors {} --workers-per-actor {} --steps {}{}{}",
            shell_quote_path(&self.executable),
            shell_quote_path(&self.component),
            shell_quote_path(&self.preview),
            self.profile.name(),
            self.seed,
            shell_quote_path(&self.out_dir),
            self.actors,
            self.workers_per_actor,
            self.steps,
            self.epochs
                .map(|epochs| format!(" --epochs {epochs}"))
                .unwrap_or_default(),
            self.duration
                .map(|duration| format!(" --duration-seconds {}", duration.as_secs()))
                .unwrap_or_default(),
        )
    }
}

async fn load_component(settings: &Settings, slot: usize) -> Result<ComponentNode> {
    let root = prepare_root(slot)?;
    let actor = ComponentActor::load(
        &settings.component,
        ComponentActorConfig {
            filesystem: Some(FilesystemGrant {
                host_path: root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: true,
            }),
            max_workers: settings.workers_per_actor.saturating_add(16),
            command_capacity: 256,
            mailbox_backlog: 64,
            dispatcher_deadline: Duration::from_millis(250),
            ..ComponentActorConfig::default()
        },
    )
    .await
    .with_context(|| format!("load Component actor in slot {slot}"))?;
    Ok(ComponentNode {
        slot,
        root,
        actor,
        messages: 0,
        dropped: 0,
    })
}

async fn load_preview(settings: &Settings, slot: usize) -> Result<PreviewNode> {
    let root = prepare_root(slot)?;
    let actor = Preview1WorkerRuntime::load(
        &settings.preview,
        Preview1ActorCapabilities {
            filesystem: Some(Preview1FilesystemGrant {
                host_path: root.path().to_path_buf(),
                guest_path: ".".to_string(),
                writable: true,
            }),
            ..Preview1ActorCapabilities::default()
        },
        Preview1WorkerConfig {
            max_workers: settings.workers_per_actor.saturating_add(16),
            command_capacity: 256,
            mailbox_backlog: 64,
            actor_wall_timeout: Duration::from_millis(250),
            ..Preview1WorkerConfig::default()
        },
    )
    .await
    .with_context(|| format!("load Preview 1 actor in slot {slot}"))?;
    Ok(PreviewNode {
        slot,
        root,
        actor,
        messages: 0,
        rube_receipts: 0,
    })
}

fn prepare_root(slot: usize) -> Result<TempDir> {
    let root = tempfile::Builder::new()
        .prefix(&format!("manifold-io-rube-{slot}-"))
        .tempdir()
        .context("create actor capability root")?;
    let contents = format!("actor-slot={slot}\n");
    fs::write(root.path().join("input.txt"), contents.as_bytes())?;
    fs::write(root.path().join("worker-input.txt"), contents.as_bytes())?;
    fs::write(root.path().join("completion.txt"), b"completion-data")?;
    create_fifo(&root.path().join("cancel.pipe"))?;
    Ok(root)
}

fn create_fifo(path: &Path) -> Result<()> {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .with_context(|| format!("run mkfifo for {}", path.display()))?;
    ensure!(status.success(), "mkfifo failed for {}", path.display());
    Ok(())
}

async fn boot_component_workers(nodes: &mut [ComponentNode], workers: usize) -> Result<()> {
    for node in nodes {
        let mut pending = BTreeSet::new();
        for _worker in 0..workers {
            pending.insert(node.actor.try_spawn_worker(b"input.txt".to_vec())?);
        }
        timeout(Duration::from_secs(10), async {
            while !pending.is_empty() {
                match node
                    .actor
                    .recv_event()
                    .await
                    .context("Component event lane closed during worker boot")?
                {
                    ComponentActorEvent::WorkerFinished { worker, output }
                        if pending.remove(&worker) =>
                    {
                        ensure!(
                            !output.is_empty(),
                            "Component worker returned an empty file"
                        );
                    }
                    ComponentActorEvent::WorkerFailed { worker, message }
                        if pending.contains(&worker) =>
                    {
                        bail!("Component boot worker {} failed: {message}", worker.raw());
                    }
                    ComponentActorEvent::DispatcherFailed { message } => {
                        bail!("Component dispatcher failed during worker boot: {message}");
                    }
                    _ => {}
                }
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .with_context(|| format!("Component slot {} worker boot timed out", node.slot))??;
    }
    Ok(())
}

async fn boot_preview_workers(nodes: &mut [PreviewNode], workers: usize) -> Result<()> {
    for node in nodes {
        let mut pending = Vec::with_capacity(workers);
        for _worker in 0..workers {
            pending.push(node.actor.spawn_worker("worker_file_roundtrip", 0).await?);
        }
        for worker in pending {
            let bytes = timeout(Duration::from_secs(10), worker.join())
                .await
                .context("Preview 1 boot worker timed out")??;
            ensure!(bytes > 0, "Preview 1 worker wrote no filesystem bytes");
        }
    }
    Ok(())
}

async fn run_component_barriers(nodes: &mut [ComponentNode]) -> Result<()> {
    let Some(node) = nodes.first_mut() else {
        bail!("Component barrier requires a Component actor");
    };

    let write =
        node.actor
            .completion_io()
            .submit_write("barrier.bin", 0, b"completion-write".to_vec())?;
    let completion = wait_component_completion(&mut node.actor, write).await?;
    ensure!(
        matches!(
            completion.kind,
            ActorCompletionKind::Write { bytes_written: 16 }
        ),
        "completion write returned the wrong terminal value: {completion:?}"
    );

    let read = node
        .actor
        .completion_io()
        .submit_read("barrier.bin", 0, 64)?;
    let completion = wait_component_completion(&mut node.actor, read).await?;
    ensure!(
        matches!(completion.kind, ActorCompletionKind::Read { ref bytes } if bytes == b"completion-write"),
        "completion read returned the wrong bytes: {completion:?}"
    );
    // The example guest turns every typed read completion into worker 99.
    let echoed = wait_component_worker(&mut node.actor, WorkerId::from_raw(99)).await?;
    ensure!(echoed == b"completion-write");

    let cancelled = node
        .actor
        .completion_io()
        .submit_read("cancel.pipe", 0, 64)?;
    ensure!(
        node.actor.completion_io().cancel(cancelled),
        "completion cancellation was not admitted"
    );
    let completion = wait_component_completion(&mut node.actor, cancelled).await?;
    ensure!(matches!(completion.kind, ActorCompletionKind::Cancelled));

    let failed = node
        .actor
        .completion_io()
        .submit_read("missing-file", 0, 64)?;
    let completion = wait_component_completion(&mut node.actor, failed).await?;
    ensure!(matches!(
        completion.kind,
        ActorCompletionKind::Failed { .. }
    ));

    let spinner = node.actor.try_spawn_worker(b"__spin__".to_vec())?;
    wait_component_started(&mut node.actor, spinner).await?;
    let checkpoint = timeout(Duration::from_secs(10), node.actor.recv_completion())
        .await
        .context("Component spinner checkpoint timed out")?
        .context("Component completion lane closed")?;
    ensure!(matches!(
        checkpoint.kind,
        ActorCompletionKind::Write { bytes_written: 3 }
    ));
    node.actor
        .try_send_message("io-rube-goldberg".to_string(), b"ping".to_vec())?;
    node.messages = node.messages.saturating_add(1);
    node.actor.try_cancel_worker(spinner)?;
    wait_component_cancelled(&mut node.actor, spinner).await?;

    node.actor.try_send_message(
        "io-rube-goldberg".to_string(),
        b"notification-start".to_vec(),
    )?;
    node.messages = node.messages.saturating_add(1);
    wait_component_started(&mut node.actor, WorkerId::from_raw(800)).await?;
    node.actor.try_send_message(
        "io-rube-goldberg".to_string(),
        b"notification-signal".to_vec(),
    )?;
    node.messages = node.messages.saturating_add(1);
    wait_component_notification_flow(&mut node.actor).await?;

    node.actor.try_send_message(
        "io-rube-goldberg".to_string(),
        b"notification-cancel-start".to_vec(),
    )?;
    node.messages = node.messages.saturating_add(1);
    let wait_worker = WorkerId::from_raw(802);
    wait_component_started(&mut node.actor, wait_worker).await?;
    node.actor.try_cancel_worker(wait_worker)?;
    wait_component_cancelled(&mut node.actor, wait_worker).await?;
    while node.actor.try_recv_event().is_some() {}
    Ok(())
}

async fn run_coordination_barrier(seed: u64) -> Result<()> {
    let generation = seed.max(1);
    let (hub, mut actor_events) = CoordinationHub::new(
        generation,
        CoordinationConfig {
            max_notifications: 4,
            max_pollables: 12,
            max_wait_set: 4,
        },
    )?;
    let (foreign, _foreign_events) = CoordinationHub::new(
        generation.saturating_add(1).max(1),
        CoordinationConfig {
            max_notifications: 1,
            max_pollables: 1,
            max_wait_set: 1,
        },
    )?;

    let signal_first = hub.create_notification()?;
    ensure!(hub.signal(signal_first, 3)? == 3);
    let _watch = hub.watch_actor(signal_first, 0, 0x11)?;
    let queued = timeout(Duration::from_secs(1), actor_events.recv())
        .await
        .context("signal-before-watch event timed out")?
        .context("coordination actor event lane closed")?;
    ensure!(queued == signal_first);
    let event = hub
        .take_actor_event(queued)?
        .context("signal-before-watch event disappeared")?;
    ensure!(event.generation == 3 && event.token == 0x11);

    let watch_first = hub.create_notification()?;
    let _watch = hub.watch_actor(watch_first, 0, 0x22)?;
    ensure!(hub.signal(watch_first, 1)? == 1);
    ensure!(hub.signal(watch_first, 3)? == 3);
    ensure!(hub.signal(watch_first, 2)? == 3);
    let queued = timeout(Duration::from_secs(1), actor_events.recv())
        .await
        .context("watch-before-signal event timed out")?
        .context("coordination actor event lane closed")?;
    let event = hub
        .take_actor_event(queued)?
        .context("watch-before-signal event disappeared")?;
    ensure!(event.generation == 3 && event.token == 0x22);

    let ready_notification = hub.create_notification()?;
    let idle_notification = hub.create_notification()?;
    ensure!(
        matches!(
            hub.create_notification(),
            Err(NotificationError::LimitReached)
        ),
        "notification limit was not enforced"
    );
    let _watch = hub.watch_actor(idle_notification, 0, 0x33)?;
    ensure!(hub.unwatch_actor(idle_notification)?);
    ensure!(!hub.unwatch_actor(idle_notification)?);
    ensure!(
        matches!(
            foreign.signal(signal_first, 1),
            Err(NotificationError::InvalidHandle)
        ),
        "foreign notification handle was accepted"
    );
    ensure!(
        matches!(
            hub.signal(ready_notification, 0),
            Err(NotificationError::InvalidGeneration)
        ),
        "zero notification generation was accepted"
    );

    let first = hub.notification_pollable(ready_notification, 0)?;
    let second = hub.notification_pollable(watch_first, 0)?;
    ensure!(hub.signal(ready_notification, 7)? == 7);
    let ready = hub.try_poll(&[first, second])?;
    ensure!(
        ready.len() == 2,
        "try-poll did not return both ready pollables"
    );
    let biased = hub
        .wait_any(
            &[first, second],
            Some(Duration::from_secs(1)),
            WaitMode::Biased,
        )
        .await?;
    ensure!(biased.index == 0 && biased.pollable == first);
    let mut fair = BTreeMap::new();
    for _iteration in 0..8 {
        let selected = hub
            .wait_any(
                &[first, second],
                Some(Duration::from_secs(1)),
                WaitMode::Fair,
            )
            .await?;
        let count = fair.entry(selected.pollable).or_insert(0_u32);
        *count = count.saturating_add(1);
    }
    ensure!(fair.get(&first) == Some(&4) && fair.get(&second) == Some(&4));
    ensure!(matches!(hub.try_poll(&[]), Err(PollError::EmptyWaitSet)));
    ensure!(matches!(
        hub.try_poll(&[first, first]),
        Err(PollError::DuplicatePollable)
    ));
    let extra_one = hub.notification_pollable(ready_notification, 0)?;
    let extra_two = hub.notification_pollable(ready_notification, 0)?;
    let extra_three = hub.notification_pollable(ready_notification, 0)?;
    ensure!(matches!(
        hub.try_poll(&[first, second, extra_one, extra_two, extra_three]),
        Err(PollError::LimitReached)
    ));

    let timeout_pollable = hub.notification_pollable(idle_notification, 0)?;
    ensure!(matches!(
        hub.wait_any(
            &[timeout_pollable],
            Some(Duration::from_millis(2)),
            WaitMode::Biased,
        )
        .await,
        Err(PollError::TimedOut)
    ));
    let dropped_pollable = hub.notification_pollable(idle_notification, 0)?;
    let waiting_hub = hub.clone();
    let dropped_wait = tokio::spawn(async move {
        waiting_hub
            .wait_any(&[dropped_pollable], None, WaitMode::Biased)
            .await
    });
    wait_for_coordination_registrations(&hub, 1).await?;
    hub.drop_pollable(dropped_pollable)?;
    ensure!(matches!(
        dropped_wait.await?,
        Err(PollError::InvalidPollable)
    ));

    let invalidated_pollable = hub.notification_pollable(idle_notification, 0)?;
    let waiting_hub = hub.clone();
    let invalidated_wait = tokio::spawn(async move {
        waiting_hub
            .wait_any(&[invalidated_pollable], None, WaitMode::Fair)
            .await
    });
    wait_for_coordination_registrations(&hub, 1).await?;
    hub.drop_notification(idle_notification)?;
    ensure!(matches!(
        invalidated_wait.await?,
        Err(PollError::InvalidPollable)
    ));

    for pollable in [first, second, extra_one, extra_two, extra_three] {
        hub.drop_pollable(pollable)?;
    }
    hub.drop_notification(signal_first)?;
    hub.drop_notification(watch_first)?;
    hub.drop_notification(ready_notification)?;
    let stats = hub.stats();
    ensure!(
        stats.notifications == 0
            && stats.pollables == 0
            && stats.wait_registrations == 0
            && stats.pending_actor_events == 0,
        "coordination barrier leaked resources: {stats:?}"
    );
    hub.stop();
    foreign.stop();
    Ok(())
}

async fn wait_for_coordination_registrations(hub: &CoordinationHub, expected: usize) -> Result<()> {
    timeout(Duration::from_secs(1), async {
        while hub.stats().wait_registrations != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("coordination wait registrations did not converge")?;
    Ok(())
}

async fn run_preview_barriers(nodes: &mut [PreviewNode]) -> Result<()> {
    let Some(node) = nodes.first_mut() else {
        bail!("Preview 1 barrier requires a Preview 1 actor");
    };

    let (mut input, stdin) = tokio::io::duplex(8);
    let reader = node
        .actor
        .spawn_worker_with_stdin("worker_read", 0, AsyncStdinStream::new(stdin))
        .await?;
    input.write_all(b"R").await?;
    let value = timeout(Duration::from_secs(10), reader.join())
        .await
        .context("ordinary Preview 1 stdin read timed out")??;
    ensure!(value == u32::from(b'R'));

    let spinner_baseline = node.actor.call_actor("actor_spinners_entered", 0).await?;
    let spinner = node.actor.spawn_worker("worker_spin", 0).await?;
    wait_preview_actor_at_least(
        &node.actor,
        "actor_spinners_entered",
        spinner_baseline.saturating_add(1),
    )
    .await?;
    let ping = node.actor.call_actor("actor_ping", 41).await?;
    ensure!(ping == 42, "Preview 1 actor stalled behind CPU worker");
    node.actor.cancel_worker(spinner.id()).await?;
    ensure!(matches!(
        spinner.join().await,
        Err(Preview1WorkerError::Cancelled)
    ));

    let before_notify = node.actor.stats();
    node.actor
        .try_send_message("io-rube-goldberg".to_string(), b"start-notify".to_vec())?;
    node.messages = node.messages.saturating_add(1);
    wait_preview_stats(&node.actor, |stats| {
        stats.mailbox_dispatched >= before_notify.mailbox_dispatched.saturating_add(1)
            && stats.workers_started >= before_notify.workers_started.saturating_add(2)
            && stats.wait_registrations == 4
    })
    .await?;
    node.actor
        .try_send_message("io-rube-goldberg".to_string(), b"alternate:B".to_vec())?;
    node.messages = node.messages.saturating_add(1);
    wait_preview_stats(&node.actor, |stats| {
        stats.workers_finished >= before_notify.workers_finished.saturating_add(1)
            && stats.wait_registrations == 2
    })
    .await?;
    node.actor
        .try_send_message("io-rube-goldberg".to_string(), b"queue:A".to_vec())?;
    node.messages = node.messages.saturating_add(1);
    wait_preview_stats(&node.actor, |stats| {
        stats.workers_finished >= before_notify.workers_finished.saturating_add(2)
            && stats.notifications_delivered
                >= before_notify.notifications_delivered.saturating_add(1)
            && stats.wait_registrations == 0
            && stats.pollables == 0
    })
    .await?;

    let before_stdin = node.actor.stats();
    node.actor
        .try_send_message("io-rube-goldberg".to_string(), b"start-stdin".to_vec())?;
    node.messages = node.messages.saturating_add(1);
    wait_preview_stats(&node.actor, |stats| {
        stats.worker_reads_started >= before_stdin.worker_reads_started.saturating_add(1)
    })
    .await?;
    node.actor
        .try_send_message("io-rube-goldberg".to_string(), b"feed:Z".to_vec())?;
    node.messages = node.messages.saturating_add(1);
    wait_preview_stats(&node.actor, |stats| {
        stats.completion_delivered >= before_stdin.completion_delivered.saturating_add(1)
            && stats.workers_finished >= before_stdin.workers_finished.saturating_add(1)
    })
    .await?;
    Ok(())
}

async fn run_mailbox_backpressure(
    components: &mut [ComponentNode],
    previews: &mut [PreviewNode],
) -> Result<MailboxPressureOutcome> {
    let component = run_component_mailbox_backpressure(components).await?;
    let preview = run_preview_mailbox_backpressure(previews).await?;
    Ok(MailboxPressureOutcome {
        component_accepted: component.0,
        component_rejected: component.1,
        preview_accepted: preview.0,
        preview_rejected: preview.1,
    })
}

async fn run_component_mailbox_backpressure(nodes: &mut [ComponentNode]) -> Result<(u64, u64)> {
    let Some(node) = nodes.last_mut() else {
        bail!("mailbox pressure barrier requires a Component actor");
    };
    while node.actor.try_recv_event().is_some() {}
    let mut accepted = 0_u64;
    let mut rejected = 0_u64;
    for sequence in 0_u64..512 {
        let body = format!("pressure-{sequence}").into_bytes();
        match node
            .actor
            .try_send_message("io-rube-pressure".to_string(), body)
        {
            Ok(()) => accepted = accepted.saturating_add(1),
            Err(_error) => rejected = rejected.saturating_add(1),
        }
    }
    ensure!(accepted > 0, "mailbox pressure admitted no messages");
    ensure!(
        rejected > 0,
        "mailbox pressure produced no bounded rejection"
    );
    node.messages = node.messages.saturating_add(accepted);
    node.dropped = node.dropped.saturating_add(rejected);
    timeout(Duration::from_secs(10), async {
        let mut dispatched = 0_u64;
        while dispatched < accepted {
            match node
                .actor
                .recv_event()
                .await
                .context("Component event lane closed")?
            {
                ComponentActorEvent::DispatcherReturned => {
                    dispatched = dispatched.saturating_add(1);
                }
                ComponentActorEvent::DispatcherFailed { message } => {
                    bail!("Component dispatcher failed during mailbox pressure: {message}");
                }
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("mailbox pressure did not drain")??;
    Ok((accepted, rejected))
}

async fn run_preview_mailbox_backpressure(nodes: &mut [PreviewNode]) -> Result<(u64, u64)> {
    let Some(node) = nodes.last_mut() else {
        bail!("mailbox pressure barrier requires a Preview 1 actor");
    };
    assert_preview_receipt_count(node).await?;
    let before = node.actor.stats();
    let mut accepted = 0_u64;
    let mut rejected = 0_u64;
    for sequence in 0_u64..512 {
        let body = format!("pressure-preview-{sequence}").into_bytes();
        match node
            .actor
            .try_send_message("io-rube-pressure".to_string(), body)
        {
            Ok(()) => accepted = accepted.saturating_add(1),
            Err(Preview1WorkerError::AtCapacity(_)) => {
                rejected = rejected.saturating_add(1);
            }
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(
        accepted > 0,
        "Preview 1 mailbox pressure admitted no messages"
    );
    ensure!(
        rejected > 0,
        "Preview 1 mailbox pressure produced no bounded rejection"
    );
    node.messages = node.messages.saturating_add(accepted);
    wait_preview_stats(&node.actor, |stats| {
        stats.mailbox_dispatched == node.messages && stats.workers_active == 0
    })
    .await?;
    let after = node.actor.stats();
    ensure!(
        after.mailbox_queued == node.messages
            && after.mailbox_dropped == before.mailbox_dropped.saturating_add(rejected),
        "Preview 1 mailbox pressure accounting mismatch: before={before:?}, after={after:?}, accepted={accepted}, rejected={rejected}"
    );
    Ok((accepted, rejected))
}

#[allow(
    clippy::significant_drop_tightening,
    reason = "replacement keeps each mutable actor guard through its complete stop-and-reload proof"
)]
async fn run_replacement_barrier(
    settings: &Settings,
    components: &mut [ComponentNode],
    previews: &mut [PreviewNode],
    ledger: &mut ReceiptLedger,
) -> Result<ReplacementOutcome> {
    let component_index = usize::from(components.len() > 1);
    let component = components
        .get_mut(component_index)
        .context("replacement barrier requires a Component actor")?;
    let old_generation = component.actor.generation();
    let old_operation = component
        .actor
        .completion_io()
        .submit_read("cancel.pipe", 0, 64)?;
    component.actor.try_send_message(
        "io-rube-replacement".to_string(),
        b"notification-cancel-start".to_vec(),
    )?;
    component.messages = component.messages.saturating_add(1);
    wait_component_started(&mut component.actor, WorkerId::from_raw(802)).await?;
    let mailbox = component.actor.mailbox_stats();
    ensure!(
        mailbox.enqueued_messages == component.messages
            && mailbox.dropped_messages == component.dropped
    );
    drain_component_receipts(component, ledger).await?;
    component.actor.shutdown().await?;
    let old_io = component.actor.completion_io().stats();
    let old_coordination = component.actor.coordination_stats();
    ensure!(old_io.outstanding == 0 && !old_io.alive);
    ensure!(
        old_coordination.notifications == 0
            && old_coordination.pollables == 0
            && old_coordination.wait_registrations == 0
            && old_coordination.pending_actor_events == 0
    );
    let slot = component.slot;
    *component = load_component(settings, slot).await?;
    ensure!(component.actor.generation() != old_generation);
    ensure!(
        !component.actor.completion_io().cancel(old_operation),
        "replacement accepted an old-generation completion operation"
    );
    let component_transition = u64::MAX - 1;
    let component_generation = component.actor.generation();
    let component_output = rube_mix(settings.seed, component_transition, component_generation);
    let component_plan = TransitionPlan {
        id: component_transition,
        epoch: u64::MAX,
        actor_slot: component.slot,
        backend: Backend::Component,
        generation: component_generation,
        input: settings.seed,
        output: component_output,
    };
    ledger.begin(component_plan)?;
    component.actor.try_send_message(
        "io-rube-replacement".to_string(),
        encode_message(
            component_transition,
            settings.seed,
            component_generation,
            component_output,
        ),
    )?;
    component.messages = component.messages.saturating_add(1);
    wait_component_receipt(component, ledger, component_plan).await?;

    let preview_index = usize::from(previews.len() > 1);
    let preview = previews
        .get_mut(preview_index)
        .context("replacement barrier requires a Preview 1 actor")?;
    let old_generation = preview.actor.stats().generation;
    wait_preview_rube_quiescent(preview).await?;
    preview
        .actor
        .try_send_message("io-rube-replacement".to_string(), b"start-notify".to_vec())?;
    preview.messages = preview.messages.saturating_add(1);
    wait_preview_stats(&preview.actor, |stats| stats.wait_registrations == 4).await?;
    ensure!(preview.actor.stats().mailbox_queued == preview.messages);
    preview.actor.shutdown().await?;
    let stopped = preview.actor.stats();
    ensure!(
        stopped.notifications == 0
            && stopped.pollables == 0
            && stopped.wait_registrations == 0
            && stopped.pending_notification_events == 0
            && stopped.workers_active == 0
    );
    let slot = preview.slot;
    *preview = load_preview(settings, slot).await?;
    let replacement_generation = preview.actor.stats().generation;
    ensure!(replacement_generation != old_generation);
    let filesystem_worker = preview
        .actor
        .spawn_worker("worker_file_roundtrip", 0)
        .await?;
    let filesystem_bytes = timeout(Duration::from_secs(10), filesystem_worker.join())
        .await
        .context("replacement Preview filesystem worker timed out")??;
    ensure!(filesystem_bytes > 0);
    let preview_transition = u64::MAX;
    let preview_output = rube_mix(settings.seed, preview_transition, replacement_generation);
    let preview_plan = TransitionPlan {
        id: preview_transition,
        epoch: u64::MAX,
        actor_slot: preview.slot,
        backend: Backend::Preview1,
        generation: replacement_generation,
        input: settings.seed,
        output: preview_output,
    };
    ledger.begin(preview_plan)?;
    assert_preview_receipt_count(preview).await?;
    preview.actor.try_send_message(
        "io-rube-replacement".to_string(),
        encode_message(
            preview_transition,
            settings.seed,
            replacement_generation,
            preview_output,
        ),
    )?;
    preview.messages = preview.messages.saturating_add(1);
    wait_preview_receipt(preview, ledger, preview_plan).await?;
    Ok(ReplacementOutcome {
        messages: 4,
        receipts: 2,
    })
}

async fn wait_component_worker(actor: &mut ComponentActor, expected: WorkerId) -> Result<Vec<u8>> {
    timeout(Duration::from_secs(10), async {
        loop {
            match actor
                .recv_event()
                .await
                .context("Component event lane closed")?
            {
                ComponentActorEvent::WorkerFinished { worker, output } if worker == expected => {
                    return Ok(output);
                }
                ComponentActorEvent::WorkerFailed { worker, message } if worker == expected => {
                    bail!("Component worker {} failed: {message}", worker.raw());
                }
                ComponentActorEvent::DispatcherFailed { message } => {
                    bail!("Component dispatcher failed: {message}");
                }
                _ => {}
            }
        }
    })
    .await
    .context("Component worker receipt timed out")?
}

async fn wait_component_completion(
    actor: &mut ComponentActor,
    expected: OperationId,
) -> Result<ActorCompletion> {
    timeout(Duration::from_secs(10), async {
        loop {
            let completion = actor
                .recv_completion()
                .await
                .context("Component completion lane closed")?;
            if completion.operation == expected {
                return Ok(completion);
            }
        }
    })
    .await
    .context("Component completion timed out")?
}

async fn wait_component_started(actor: &mut ComponentActor, expected: WorkerId) -> Result<()> {
    timeout(Duration::from_secs(10), async {
        loop {
            match actor
                .recv_event()
                .await
                .context("Component event lane closed")?
            {
                ComponentActorEvent::WorkerStarted { worker } if worker == expected => {
                    return Ok(());
                }
                ComponentActorEvent::WorkerFailed { worker, message } if worker == expected => {
                    bail!(
                        "Component worker {} failed during start: {message}",
                        worker.raw()
                    );
                }
                ComponentActorEvent::DispatcherFailed { message } => {
                    bail!("Component dispatcher failed: {message}");
                }
                _ => {}
            }
        }
    })
    .await
    .context("Component worker start timed out")?
}

async fn wait_component_cancelled(actor: &mut ComponentActor, expected: WorkerId) -> Result<()> {
    timeout(Duration::from_secs(10), async {
        loop {
            match actor
                .recv_event()
                .await
                .context("Component event lane closed")?
            {
                ComponentActorEvent::WorkerCancelled { worker } if worker == expected => {
                    return Ok(());
                }
                ComponentActorEvent::WorkerFailed { worker, message } if worker == expected => {
                    bail!(
                        "Component worker {} failed during cancellation: {message}",
                        worker.raw()
                    );
                }
                ComponentActorEvent::DispatcherFailed { message } => {
                    bail!("Component dispatcher failed: {message}");
                }
                _ => {}
            }
        }
    })
    .await
    .context("Component worker cancellation timed out")?
}

async fn wait_component_notification_flow(actor: &mut ComponentActor) -> Result<()> {
    let waiting = WorkerId::from_raw(800);
    let acknowledgement = WorkerId::from_raw(801);
    timeout(Duration::from_secs(10), async {
        let mut saw_waiting = false;
        let mut saw_acknowledgement = false;
        let mut saw_notification = false;
        while !(saw_waiting && saw_acknowledgement && saw_notification) {
            match actor
                .recv_event()
                .await
                .context("Component event lane closed")?
            {
                ComponentActorEvent::WorkerFinished { worker, output } if worker == waiting => {
                    ensure!(output == b"Z");
                    saw_waiting = true;
                }
                ComponentActorEvent::WorkerFinished { worker, output }
                    if worker == acknowledgement =>
                {
                    ensure!(output == b"notification-ok");
                    saw_acknowledgement = true;
                }
                ComponentActorEvent::NotificationDelivered {
                    generation: 3,
                    token: 0xcafe,
                    ..
                } => saw_notification = true,
                ComponentActorEvent::WorkerFailed { worker, message }
                    if worker == waiting || worker == acknowledgement =>
                {
                    bail!(
                        "Component notification worker {} failed: {message}",
                        worker.raw()
                    );
                }
                ComponentActorEvent::DispatcherFailed { message } => {
                    bail!("Component dispatcher failed: {message}");
                }
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("Component notification flow timed out")??;
    Ok(())
}

async fn wait_preview_actor_at_least(
    actor: &Preview1WorkerRuntime,
    export: &str,
    expected: u32,
) -> Result<()> {
    timeout(Duration::from_secs(10), async {
        loop {
            if actor.call_actor(export, 0).await? >= expected {
                return Ok::<(), Preview1WorkerError>(());
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .with_context(|| format!("Preview 1 export {export} did not reach {expected}"))??;
    Ok(())
}

async fn wait_preview_stats(
    actor: &Preview1WorkerRuntime,
    ready: impl Fn(manifold_wasm::Preview1CompatStats) -> bool + Send + Sync,
) -> Result<()> {
    timeout(Duration::from_secs(10), async {
        loop {
            if ready(actor.stats()) {
                return;
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .context("Preview 1 lifecycle counters did not converge")?;
    Ok(())
}

async fn wait_component_receipt(
    node: &mut ComponentNode,
    ledger: &mut ReceiptLedger,
    plan: TransitionPlan,
) -> Result<()> {
    timeout(Duration::from_secs(10), async {
        loop {
            match node
                .actor
                .recv_event()
                .await
                .context("Component event lane closed")?
            {
                ComponentActorEvent::WorkerFinished { worker, output }
                    if worker.raw() >= COMPONENT_WORKER_BASE =>
                {
                    let transition = ledger.finish_component(
                        worker,
                        node.slot,
                        node.actor.generation(),
                        &output,
                    )?;
                    ensure!(
                        transition == plan.id,
                        "out-of-order Component receipt: expected transition {}, got {transition}",
                        plan.id
                    );
                    return Ok::<(), anyhow::Error>(());
                }
                ComponentActorEvent::WorkerFailed { worker, message }
                    if worker.raw() >= COMPONENT_WORKER_BASE =>
                {
                    bail!(
                        "Component Rube worker {} failed while transition {} was pending: {message}",
                        worker.raw(),
                        plan.id
                    );
                }
                ComponentActorEvent::WorkerCancelled { worker }
                    if worker.raw() >= COMPONENT_WORKER_BASE =>
                {
                    bail!(
                        "Component Rube worker {} was cancelled while transition {} was pending",
                        worker.raw(),
                        plan.id
                    );
                }
                ComponentActorEvent::DispatcherFailed { message } => {
                    bail!("Component dispatcher failed: {message}");
                }
                _ => {}
            }
        }
    })
    .await
    .with_context(|| format!("Component transition {} receipt timed out", plan.id))??;
    Ok(())
}

async fn wait_preview_receipt(
    node: &mut PreviewNode,
    ledger: &mut ReceiptLedger,
    plan: TransitionPlan,
) -> Result<()> {
    let expected_completed = node
        .rube_receipts
        .checked_add(1)
        .context("Preview 1 Rube receipt counter exhausted")?;
    timeout(Duration::from_secs(10), async {
        loop {
            let completed = node.actor.call_actor("actor_rube_completed", 0).await?;
            ensure!(
                completed <= expected_completed,
                "duplicate or unordered Preview 1 receipts: expected counter {expected_completed}, got {completed}"
            );
            if completed == expected_completed {
                let error = node.actor.call_actor("actor_rube_error", 0).await?;
                ensure!(error == 0, "Preview 1 guest reported Rube error {error}");
                let low = node.actor.call_actor("actor_rube_output_low", 0).await?;
                let high = node.actor.call_actor("actor_rube_output_high", 0).await?;
                let output = u64::from(low) | (u64::from(high) << 32);
                let stable_completed = node.actor.call_actor("actor_rube_completed", 0).await?;
                ensure!(
                    stable_completed == expected_completed,
                    "duplicate Preview 1 receipt arrived while reading transition {} output",
                    plan.id
                );
                ledger.finish_preview(
                    plan.id,
                    node.slot,
                    node.actor.stats().generation,
                    output,
                )?;
                node.rube_receipts = expected_completed;
                return Ok::<(), anyhow::Error>(());
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .context("Preview 1 transition receipt timed out")??;
    wait_preview_rube_quiescent(node).await?;
    Ok(())
}

async fn assert_preview_receipt_count(node: &PreviewNode) -> Result<()> {
    let completed = node.actor.call_actor("actor_rube_completed", 0).await?;
    ensure!(
        completed == node.rube_receipts,
        "Preview 1 slot {} receipt ledger drifted: expected {}, got {completed}",
        node.slot,
        node.rube_receipts
    );
    Ok(())
}

async fn wait_preview_rube_quiescent(node: &PreviewNode) -> Result<()> {
    wait_preview_stats(&node.actor, |stats| stats.workers_active == 0).await?;
    assert_preview_receipt_count(node).await
}

async fn drain_component_receipts(
    node: &mut ComponentNode,
    ledger: &mut ReceiptLedger,
) -> Result<()> {
    tokio::task::yield_now().await;
    while let Some(event) = node.actor.try_recv_event() {
        match event {
            ComponentActorEvent::WorkerFinished { worker, output }
                if worker.raw() >= COMPONENT_WORKER_BASE =>
            {
                let transition =
                    ledger.finish_component(worker, node.slot, node.actor.generation(), &output)?;
                bail!("unexpected late Component receipt for transition {transition}");
            }
            ComponentActorEvent::WorkerFailed { worker, message }
                if worker.raw() >= COMPONENT_WORKER_BASE =>
            {
                bail!(
                    "late Component Rube worker {} failure: {message}",
                    worker.raw()
                );
            }
            ComponentActorEvent::WorkerCancelled { worker }
                if worker.raw() >= COMPONENT_WORKER_BASE =>
            {
                bail!("late Component Rube worker {} cancellation", worker.raw());
            }
            ComponentActorEvent::DispatcherFailed { message } => {
                bail!("Component dispatcher failed: {message}");
            }
            _ => {}
        }
    }
    Ok(())
}

async fn verify_no_extra_component_receipts(
    nodes: &mut [ComponentNode],
    ledger: &mut ReceiptLedger,
) -> Result<()> {
    for node in nodes {
        drain_component_receipts(node, ledger).await?;
    }
    Ok(())
}

async fn shutdown_components(nodes: &mut [ComponentNode]) -> Result<()> {
    let mut failures = Vec::new();
    for node in nodes {
        let mailbox = node.actor.mailbox_stats();
        if mailbox.enqueued_messages != node.messages || mailbox.dropped_messages != node.dropped {
            failures.push(format!(
                "Component slot {} mailbox accounting mismatch: {mailbox:?}",
                node.slot
            ));
        }
        if let Err(error) = node.actor.shutdown().await {
            failures.push(format!(
                "Component slot {} shutdown failed: {error:#}",
                node.slot
            ));
        }
        while let Some(event) = node.actor.try_recv_event() {
            match event {
                ComponentActorEvent::WorkerFinished { worker, .. }
                    if worker.raw() >= COMPONENT_WORKER_BASE =>
                {
                    failures.push(format!(
                        "Component slot {} produced a late or duplicate receipt from worker {} during shutdown",
                        node.slot,
                        worker.raw()
                    ));
                }
                ComponentActorEvent::WorkerFailed { worker, message }
                    if worker.raw() >= COMPONENT_WORKER_BASE =>
                {
                    failures.push(format!(
                        "Component slot {} Rube worker {} failed during shutdown: {message}",
                        node.slot,
                        worker.raw()
                    ));
                }
                ComponentActorEvent::WorkerCancelled { worker }
                    if worker.raw() >= COMPONENT_WORKER_BASE =>
                {
                    failures.push(format!(
                        "Component slot {} Rube worker {} was cancelled during shutdown",
                        node.slot,
                        worker.raw()
                    ));
                }
                _ => {}
            }
        }
        let completion = node.actor.completion_io().stats();
        if completion.outstanding != 0 || completion.alive {
            failures.push(format!(
                "Component slot {} leaked completion operations: {completion:?}",
                node.slot
            ));
        }
        let coordination = node.actor.coordination_stats();
        if coordination.notifications != 0
            || coordination.pollables != 0
            || coordination.wait_registrations != 0
            || coordination.pending_actor_events != 0
        {
            failures.push(format!(
                "Component slot {} leaked coordination resources: {coordination:?}",
                node.slot
            ));
        }
        if !node.root.path().join("input.txt").is_file() {
            failures.push(format!(
                "Component slot {} capability root disappeared",
                node.slot
            ));
        }
    }
    finish_shutdown_failures("Component", &failures)
}

async fn shutdown_previews(nodes: &mut [PreviewNode]) -> Result<()> {
    let mut failures = Vec::new();
    for node in nodes {
        let before = node.actor.stats();
        if before.mailbox_queued != node.messages || before.mailbox_dispatched != node.messages {
            failures.push(format!(
                "Preview 1 slot {} mailbox accounting mismatch: {before:?}",
                node.slot
            ));
        }
        if before.workers_active == 0
            && let Err(error) = assert_preview_receipt_count(node).await
        {
            failures.push(format!(
                "Preview 1 slot {} receipt accounting failed before shutdown: {error:#}",
                node.slot
            ));
        }
        if let Err(error) = node.actor.shutdown().await {
            failures.push(format!(
                "Preview 1 slot {} shutdown failed: {error:#}",
                node.slot
            ));
        }
        let after = node.actor.stats();
        if after.notifications != 0
            || after.pollables != 0
            || after.wait_registrations != 0
            || after.pending_notification_events != 0
            || after.workers_active != 0
        {
            failures.push(format!(
                "Preview 1 slot {} leaked runtime resources: {after:?}",
                node.slot
            ));
        }
        if !node.root.path().is_dir() {
            failures.push(format!(
                "Preview 1 slot {} capability root disappeared",
                node.slot
            ));
        }
    }
    finish_shutdown_failures("Preview 1", &failures)
}

async fn shutdown_all(
    components: &mut [ComponentNode],
    previews: &mut [PreviewNode],
) -> Result<()> {
    let component_result = shutdown_components(components).await;
    let preview_result = shutdown_previews(previews).await;
    match (component_result, preview_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(component), Ok(())) => Err(component),
        (Ok(()), Err(preview)) => Err(preview),
        (Err(component), Err(preview)) => Err(anyhow::anyhow!(
            "Component shutdown failed: {component:#}; Preview 1 shutdown failed: {preview:#}"
        )),
    }
}

fn finish_shutdown_failures(kind: &str, failures: &[String]) -> Result<()> {
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{kind} shutdown failures: {}", failures.join("; "))
    }
}

fn combine_workload_and_shutdown<T>(workload: Result<T>, shutdown: Result<()>) -> Result<T> {
    match (workload, shutdown) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(workload), Ok(())) => Err(workload),
        (Ok(_value), Err(shutdown)) => Err(shutdown),
        (Err(workload), Err(shutdown)) => Err(anyhow::anyhow!(
            "workload failed: {workload:#}; structured shutdown also failed: {shutdown:#}"
        )),
    }
}

fn resource_totals(components: &[ComponentNode], previews: &[PreviewNode]) -> ResourceTotals {
    let mut totals = ResourceTotals::default();
    for node in components {
        let coordination = node.actor.coordination_stats();
        totals.notifications = totals
            .notifications
            .saturating_add(coordination.notifications);
        totals.pollables = totals.pollables.saturating_add(coordination.pollables);
        totals.wait_registrations = totals
            .wait_registrations
            .saturating_add(coordination.wait_registrations);
        totals.pending_actor_events = totals
            .pending_actor_events
            .saturating_add(coordination.pending_actor_events);
        totals.completion_outstanding = totals
            .completion_outstanding
            .saturating_add(node.actor.completion_io().stats().outstanding);
    }
    for node in previews {
        let stats = node.actor.stats();
        totals.notifications = totals.notifications.saturating_add(stats.notifications);
        totals.pollables = totals.pollables.saturating_add(stats.pollables);
        totals.wait_registrations = totals
            .wait_registrations
            .saturating_add(stats.wait_registrations);
        totals.pending_actor_events = totals
            .pending_actor_events
            .saturating_add(stats.pending_notification_events);
        totals.preview_workers_active = totals
            .preview_workers_active
            .saturating_add(stats.workers_active);
    }
    totals
}

fn encode_message(transition: u64, input: u64, generation: u64, output: u64) -> Vec<u8> {
    let mut message = Vec::with_capacity(40);
    message.extend_from_slice(MESSAGE_MAGIC);
    message.extend_from_slice(&transition.to_le_bytes());
    message.extend_from_slice(&input.to_le_bytes());
    message.extend_from_slice(&generation.to_le_bytes());
    message.extend_from_slice(&output.to_le_bytes());
    message
}

const fn component_worker_for_transition(transition: u64) -> WorkerId {
    WorkerId::from_raw(COMPONENT_WORKER_BASE.saturating_add(transition))
}

const fn rube_mix(input: u64, transition: u64, generation: u64) -> u64 {
    let mut value =
        input ^ transition.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ generation.rotate_left(17);
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn parse_u64(value: &str) -> Result<u64> {
    value.strip_prefix("0x").map_or_else(
        || value.parse::<u64>().context("parse decimal integer"),
        |hex| u64::from_str_radix(hex, 16).context("parse hexadecimal integer"),
    )
}

fn parse_usize(value: &str, option: &str) -> Result<usize> {
    let parsed = parse_u64(value).with_context(|| format!("parse {option}"))?;
    usize::try_from(parsed).with_context(|| format!("{option} does not fit in usize"))
}

fn shell_quote_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_manifest(settings: &Settings) -> Result<()> {
    let manifest = json!({
        "schema": "manifold.io-rube-goldberg.manifest.v1",
        "plan_version": PLAN_VERSION,
        "profile": settings.profile.name(),
        "seed": settings.seed,
        "topology": {
            "actors": settings.actors,
            "component_actors": settings.actors.div_ceil(2),
            "preview1_actors": settings.actors / 2,
            "workers_per_actor": settings.workers_per_actor,
        },
        "work": {
            "steps_per_epoch": settings.steps,
            "epochs": settings.epochs,
            "duration_seconds": settings.duration.map(|duration| duration.as_secs()),
        },
        "implemented_surfaces": [
            "mailbox",
            "completion-read",
            "completion-write",
            "completion-cancel",
            "completion-failure",
            "component-worker-async-file-io",
            "preview1-standard-wasi-file-io",
            "preview1-standard-wasi-stdin",
            "shared-memory-atomics",
            "notification-watch-and-signal",
            "notification-coalescing",
            "worker-wait-any-fair",
            "worker-wait-any-biased-cancellation",
            "worker-cancellation",
            "cpu-worker-yield",
            "bounded-mailbox-backpressure",
            "pollable-timeout-and-invalidation",
            "foreign-handle-rejection",
            "hot-replacement",
            "stale-generation-rejection",
            "structured-shutdown",
        ],
        "out_of_scope": {
            "subprocess": "not implemented",
            "pty": "not implemented",
            "socket": "not implemented",
            "generic_timer_pollable": "not implemented",
        },
        "replay": settings.replay_command(),
    });
    let path = settings.out_dir.join("manifest.json");
    fs::write(&path, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("write workload manifest {}", path.display()))?;
    Ok(())
}

fn write_resource_samples(
    settings: &Settings,
    phase: &str,
    components: &[ComponentNode],
    previews: &[PreviewNode],
) -> Result<()> {
    let component = components
        .iter()
        .map(|node| {
            let coordination = node.actor.coordination_stats();
            let completion = node.actor.completion_io().stats();
            json!({
                "slot": node.slot,
                "generation": node.actor.generation(),
                "mailbox": node.actor.mailbox_stats().enqueued_messages,
                "notifications": coordination.notifications,
                "pollables": coordination.pollables,
                "wait_registrations": coordination.wait_registrations,
                "pending_actor_events": coordination.pending_actor_events,
                "completion_outstanding": completion.outstanding,
                "completion_alive": completion.alive,
            })
        })
        .collect::<Vec<_>>();
    let preview = previews
        .iter()
        .map(|node| {
            let stats = node.actor.stats();
            json!({
                "slot": node.slot,
                "generation": stats.generation,
                "mailbox_queued": stats.mailbox_queued,
                "mailbox_dropped": stats.mailbox_dropped,
                "mailbox_capacity": stats.mailbox_capacity,
                "mailbox_dispatched": stats.mailbox_dispatched,
                "notifications": stats.notifications,
                "pollables": stats.pollables,
                "wait_registrations": stats.wait_registrations,
                "pending_actor_events": stats.pending_notification_events,
                "workers_started": stats.workers_started,
                "workers_active": stats.workers_active,
                "workers_finished": stats.workers_finished,
                "workers_failed": stats.workers_failed,
                "workers_cancelled": stats.workers_cancelled,
            })
        })
        .collect::<Vec<_>>();
    let sample = json!({
        "schema": "manifold.io-rube-goldberg.resources.v1",
        "phase": phase,
        "component": component,
        "preview1": preview,
    });
    let path = settings.out_dir.join("resource-samples.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open resource samples {}", path.display()))?;
    serde_json::to_writer(&mut file, &sample).context("encode resource sample")?;
    file.write_all(b"\n").context("append resource sample")?;
    Ok(())
}
