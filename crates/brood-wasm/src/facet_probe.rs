//! Measured execution probe for an isolated, host-invoked pure facet.
//!
//! This is deliberately a prototype rather than a product ABI. It exercises a
//! real Wasmtime guest in its own instance, rejects every import, bounds calls
//! with both fuel and epochs, and records copy/call timing. The runnable
//! `facet_probe` example supplies the real busy `ComponentActor` workload.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use wasmtime::{
    Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc,
};

const INPUT_OFFSET: usize = 0;
const OUTPUT_OFFSET: usize = 1 << 20;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct FacetProbeConfig {
    pub fuel_per_call: u64,
    pub epoch_tick: Duration,
    pub epoch_deadline_ticks: u64,
}

impl Default for FacetProbeConfig {
    fn default() -> Self {
        Self {
            fuel_per_call: 20_000_000,
            epoch_tick: Duration::from_millis(2),
            epoch_deadline_ticks: 50,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FacetProbeInput {
    pub label: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct FacetTransferTiming {
    pub label: String,
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub copy_in: Duration,
    pub guest_call: Duration,
    pub copy_out: Duration,
    pub fuel_consumed: u64,
    pub output_checksum: u64,
}

#[derive(Clone, Debug)]
pub struct FacetProbeReport {
    pub timings: Vec<FacetTransferTiming>,
}

struct EpochDriver {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl EpochDriver {
    fn start(engine: Engine, tick: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                thread::park_timeout(tick);
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

pub fn run_facet_probe(
    facet_wasm: &[u8],
    inputs: Vec<FacetProbeInput>,
    config: FacetProbeConfig,
) -> Result<FacetProbeReport> {
    ensure!(config.fuel_per_call > 0, "facet fuel must be non-zero");
    ensure!(!config.epoch_tick.is_zero(), "epoch tick must be non-zero");
    ensure!(
        config.epoch_deadline_ticks > 0,
        "epoch deadline must be non-zero"
    );

    let mut engine_config = Config::new();
    engine_config.consume_fuel(true).epoch_interruption(true);
    let engine = Engine::new(&engine_config)
        .map_err(|error| anyhow::anyhow!("create facet probe engine: {error}"))?;
    let facet_module = Module::new(&engine, facet_wasm)
        .map_err(|error| anyhow::anyhow!("compile facet guest: {error}"))?;
    ensure!(
        facet_module.imports().next().is_none(),
        "pure facet guest declares denied imports"
    );

    let _epochs = EpochDriver::start(engine.clone(), config.epoch_tick);
    let limits = StoreLimitsBuilder::new()
        .memory_size(4 * 1024 * 1024)
        .instances(1)
        .memories(1)
        .tables(0)
        .build();
    let mut store = Store::new(&engine, limits);
    store.limiter(|limits: &mut StoreLimits| limits);
    store
        .set_fuel(config.fuel_per_call)
        .map_err(|error| anyhow::anyhow!("set instantiation fuel: {error}"))?;
    store.set_epoch_deadline(config.epoch_deadline_ticks);
    store.epoch_deadline_trap();
    let instance = Instance::new(&mut store, &facet_module, &[])
        .map_err(|error| anyhow::anyhow!("instantiate isolated facet guest: {error}"))?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .context("facet guest must export memory")?;
    let render: TypedFunc<(u32, u32, u32), u64> = instance
        .get_typed_func(&mut store, "facet-render")
        .map_err(|error| anyhow::anyhow!("facet guest must export facet-render: {error}"))?;

    let mut timings = Vec::with_capacity(inputs.len());
    for input in inputs {
        ensure!(
            input.bytes.len() <= OUTPUT_OFFSET - INPUT_OFFSET,
            "{} input exceeds the probe transfer region",
            input.label
        );
        store
            .set_fuel(config.fuel_per_call)
            .map_err(|error| anyhow::anyhow!("reset facet fuel: {error}"))?;
        store.set_epoch_deadline(config.epoch_deadline_ticks);
        store.epoch_deadline_trap();

        let started = Instant::now();
        memory
            .write(&mut store, INPUT_OFFSET, &input.bytes)
            .with_context(|| format!("copy {} into facet", input.label))?;
        let copied_in = Instant::now();
        let packed = render
            .call(
                &mut store,
                (
                    u32::try_from(INPUT_OFFSET)?,
                    u32::try_from(input.bytes.len())?,
                    u32::try_from(OUTPUT_OFFSET)?,
                ),
            )
            .map_err(|error| anyhow::anyhow!("execute facet for {}: {error}", input.label))?;
        let called = Instant::now();
        let output_len = usize::try_from(packed >> 32)?;
        ensure!(
            output_len <= MAX_OUTPUT_BYTES,
            "{} facet output exceeds the probe output limit",
            input.label
        );
        let output_checksum = packed & u64::from(u32::MAX);
        let mut output = vec![0_u8; output_len];
        memory
            .read(&store, OUTPUT_OFFSET, &mut output)
            .with_context(|| format!("copy {} output from facet", input.label))?;
        let finished = Instant::now();
        let remaining_fuel = store.get_fuel().unwrap_or(0);
        let timing = FacetTransferTiming {
            label: input.label.clone(),
            input_bytes: input.bytes.len(),
            output_bytes: output.len(),
            copy_in: copied_in.duration_since(started),
            guest_call: called.duration_since(copied_in),
            copy_out: finished.duration_since(called),
            fuel_consumed: config.fuel_per_call.saturating_sub(remaining_fuel),
            output_checksum,
        };

        // A discarded attempt followed by the same immutable input must be
        // observationally identical. This catches hidden guest state that
        // affects the result, without treating the first output as committed.
        store
            .set_fuel(config.fuel_per_call)
            .map_err(|error| anyhow::anyhow!("reset repeat-call fuel: {error}"))?;
        store.set_epoch_deadline(config.epoch_deadline_ticks);
        let repeated = render
            .call(
                &mut store,
                (
                    u32::try_from(INPUT_OFFSET)?,
                    u32::try_from(input.bytes.len())?,
                    u32::try_from(OUTPUT_OFFSET)?,
                ),
            )
            .map_err(|error| {
                anyhow::anyhow!("repeat facet execution for {}: {error}", input.label)
            })?;
        ensure!(
            repeated == packed,
            "{} facet output changed on discard/repeat",
            input.label
        );
        let mut repeated_output = vec![0_u8; output_len];
        memory
            .read(&store, OUTPUT_OFFSET, &mut repeated_output)
            .with_context(|| format!("copy repeated {} output from facet", input.label))?;
        ensure!(
            repeated_output == output,
            "{} facet bytes changed on discard/repeat",
            input.label
        );
        timings.push(timing);
    }

    Ok(FacetProbeReport { timings })
}

pub fn verify_denied_imports(candidate_wasm: &[u8]) -> Result<()> {
    let engine = Engine::default();
    let module = Module::new(&engine, candidate_wasm)
        .map_err(|error| anyhow::anyhow!("compile facet import probe: {error}"))?;
    ensure!(
        module.imports().next().is_none(),
        "pure facet guest declares denied imports"
    );
    Ok(())
}
