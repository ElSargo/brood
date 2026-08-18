//! Capability-empty QuickJS-in-WASM runtime for computer-use tasks.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use wasmtime::{Config, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};
use wasmtime_wasi::{WasiCtxBuilder, p1};

use crate::resource_limits::GenerationStoreLimits;

const WASI_PREVIEW1_MODULE: &str = "wasi_snapshot_preview1";
const MEMORY_EXPORT: &str = "memory";
const ALLOC_EXPORT: &str = "cu_alloc";
const FREE_EXPORT: &str = "cu_free";
const EXEC_EXPORT: &str = "cu_exec";

/// Hard ceilings applied to one persistent task isolate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComputerUseJsConfig {
    pub max_source_bytes: usize,
    pub max_response_bytes: usize,
    pub max_memory_bytes: usize,
    pub max_table_elements: usize,
    pub max_host_calls: usize,
    pub max_console_lines: usize,
    pub fuel_per_call: u64,
    pub wall_time: Duration,
    pub epoch_tick: Duration,
}

impl Default for ComputerUseJsConfig {
    fn default() -> Self {
        Self {
            max_source_bytes: 256 * 1024,
            max_response_bytes: 4 * 1024 * 1024,
            max_memory_bytes: 64 * 1024 * 1024,
            max_table_elements: 16 * 1024,
            max_host_calls: 128,
            max_console_lines: 1024,
            fuel_per_call: 50_000_000,
            wall_time: Duration::from_secs(2),
            epoch_tick: Duration::from_millis(5),
        }
    }
}

impl ComputerUseJsConfig {
    fn validate(self) -> Result<()> {
        ensure!(self.max_source_bytes > 0, "source limit must be non-zero");
        ensure!(
            self.max_response_bytes > 0,
            "response limit must be non-zero"
        );
        ensure!(self.max_memory_bytes > 0, "memory limit must be non-zero");
        ensure!(self.max_table_elements > 0, "table limit must be non-zero");
        ensure!(self.max_host_calls > 0, "host-call limit must be non-zero");
        ensure!(self.max_console_lines > 0, "console limit must be non-zero");
        ensure!(self.fuel_per_call > 0, "fuel limit must be non-zero");
        ensure!(
            !self.wall_time.is_zero(),
            "wall-time limit must be non-zero"
        );
        ensure!(!self.epoch_tick.is_zero(), "epoch tick must be non-zero");
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct ComputerUseJsPlan {
    pub calls: Vec<Value>,
    pub console: Vec<String>,
    pub value_json: Option<String>,
    pub exception: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ComputerUseJsMetrics {
    pub wall_ms: u32,
    pub fuel_consumed: u64,
    pub memory_bytes: u64,
    pub host_calls: u16,
}

#[derive(Debug)]
pub struct ComputerUseJsExecution {
    pub plan: ComputerUseJsPlan,
    pub metrics: ComputerUseJsMetrics,
}

struct ComputerUseJsStore {
    wasi: p1::WasiP1Ctx,
    limits: GenerationStoreLimits,
}

struct EpochTicker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Engine, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                thread::sleep(interval);
                engine.increment_epoch();
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _result = thread.join();
        }
    }
}

/// One persistent, capability-empty `QuickJS` global environment.
pub struct ComputerUseJsIsolate {
    config: ComputerUseJsConfig,
    store: Store<ComputerUseJsStore>,
    _instance: Instance,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
    exec: TypedFunc<(u32, u32), u64>,
    _epoch_ticker: EpochTicker,
}

impl std::fmt::Debug for ComputerUseJsIsolate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ComputerUseJsIsolate")
            .field("config", &self.config)
            .field("memory_bytes", &self.memory.data_size(&self.store))
            .finish_non_exhaustive()
    }
}

impl ComputerUseJsIsolate {
    /// Instantiate a `QuickJS` guest whose only allowed imports are WASI Preview
    /// 1 functions backed by an empty context.
    pub fn load(module_bytes: &[u8], config: ComputerUseJsConfig) -> Result<Self> {
        config.validate()?;
        let mut engine_config = Config::new();
        engine_config.consume_fuel(true).epoch_interruption(true);
        let engine = Engine::new(&engine_config)
            .map_err(|error| anyhow::anyhow!("create computer-use JS engine: {error}"))?;
        let module = Module::new(&engine, module_bytes)
            .map_err(|error| anyhow::anyhow!("compile QuickJS WASM module: {error}"))?;
        for import in module.imports() {
            ensure!(
                import.module() == WASI_PREVIEW1_MODULE,
                "QuickJS module requests unauthorized import {}::{}",
                import.module(),
                import.name()
            );
        }

        let mut linker = Linker::new(&engine);
        p1::add_to_linker_sync(&mut linker, |store: &mut ComputerUseJsStore| {
            &mut store.wasi
        })
        .map_err(|error| anyhow::anyhow!("link capability-empty WASI context: {error}"))?;
        let limits = GenerationStoreLimits::new(config.max_memory_bytes, config.max_table_elements);
        let mut store = Store::new(
            &engine,
            ComputerUseJsStore {
                wasi: WasiCtxBuilder::new().build_p1(),
                limits,
            },
        );
        store.limiter(|data| &mut data.limits);
        store.set_fuel(config.fuel_per_call)?;
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|error| anyhow::anyhow!("instantiate QuickJS WASM module: {error}"))?;
        let memory = instance
            .get_memory(&mut store, MEMORY_EXPORT)
            .context("QuickJS guest does not export memory")?;
        let alloc = instance
            .get_typed_func(&mut store, ALLOC_EXPORT)
            .map_err(|error| anyhow::anyhow!("QuickJS guest does not export cu_alloc: {error}"))?;
        let free = instance
            .get_typed_func(&mut store, FREE_EXPORT)
            .map_err(|error| anyhow::anyhow!("QuickJS guest does not export cu_free: {error}"))?;
        let exec = instance
            .get_typed_func(&mut store, EXEC_EXPORT)
            .map_err(|error| anyhow::anyhow!("QuickJS guest does not export cu_exec: {error}"))?;
        let epoch_ticker = EpochTicker::start(engine, config.epoch_tick);
        Ok(Self {
            config,
            store,
            _instance: instance,
            memory,
            alloc,
            free,
            exec,
            _epoch_ticker: epoch_ticker,
        })
    }

    /// Evaluate source in the persistent global environment and return only a
    /// bounded typed host-call plan. No compositor capability enters the guest.
    pub fn execute(&mut self, source: &str) -> Result<ComputerUseJsExecution> {
        ensure!(!source.is_empty(), "script source is empty");
        ensure!(
            source.len() <= self.config.max_source_bytes,
            "script source exceeds {} byte limit",
            self.config.max_source_bytes
        );
        self.store.set_fuel(self.config.fuel_per_call)?;
        let deadline_ticks = self
            .config
            .wall_time
            .as_nanos()
            .div_ceil(self.config.epoch_tick.as_nanos())
            .max(1);
        self.store
            .set_epoch_deadline(u64::try_from(deadline_ticks).unwrap_or(u64::MAX));
        self.store.epoch_deadline_trap();

        let started = Instant::now();
        let source_len =
            u32::try_from(source.len()).context("script source length overflows ABI")?;
        let source_pointer = self
            .alloc
            .call(&mut self.store, source_len)
            .map_err(|error| anyhow::anyhow!("allocate guest script input: {error}"))?;
        ensure!(
            source_pointer != 0,
            "guest rejected script input allocation"
        );
        self.memory
            .write(&mut self.store, source_pointer as usize, source.as_bytes())
            .context("write script source into guest memory")?;
        let packed = match self
            .exec
            .call(&mut self.store, (source_pointer, source_len))
        {
            Ok(packed) => packed,
            Err(error) => {
                let _result = self
                    .free
                    .call(&mut self.store, (source_pointer, source_len));
                bail!("execute QuickJS source: {error}");
            }
        };
        self.free
            .call(&mut self.store, (source_pointer, source_len))
            .map_err(|error| anyhow::anyhow!("release guest script input: {error}"))?;

        let response_pointer = u32::try_from(packed & u64::from(u32::MAX))
            .context("response pointer overflows ABI")?;
        let response_len = u32::try_from(packed >> 32).context("response length overflows ABI")?;
        ensure!(response_pointer != 0, "guest returned a null response");
        if response_len as usize > self.config.max_response_bytes {
            let _result = self
                .free
                .call(&mut self.store, (response_pointer, response_len));
            bail!(
                "guest response exceeds {} byte limit",
                self.config.max_response_bytes
            );
        }
        let mut response = vec![0_u8; response_len as usize];
        self.memory
            .read(&self.store, response_pointer as usize, &mut response)
            .context("read guest script response")?;
        self.free
            .call(&mut self.store, (response_pointer, response_len))
            .map_err(|error| anyhow::anyhow!("release guest script response: {error}"))?;
        let plan: ComputerUseJsPlan =
            serde_json::from_slice(&response).context("decode guest script response")?;
        ensure!(
            plan.calls.len() <= self.config.max_host_calls,
            "script queued more than {} host calls",
            self.config.max_host_calls
        );
        ensure!(
            plan.console.len() <= self.config.max_console_lines,
            "script emitted more than {} console lines",
            self.config.max_console_lines
        );
        let host_calls = u16::try_from(plan.calls.len()).unwrap_or(u16::MAX);
        let wall_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let remaining_fuel = self.store.get_fuel().unwrap_or(0);
        let memory_bytes = u64::try_from(self.memory.data_size(&self.store)).unwrap_or(u64::MAX);
        if memory_bytes > u64::try_from(self.config.max_memory_bytes).unwrap_or(u64::MAX) {
            bail!("guest memory exceeded configured limit");
        }
        Ok(ComputerUseJsExecution {
            plan,
            metrics: ComputerUseJsMetrics {
                wall_ms,
                fuel_consumed: self.config.fuel_per_call.saturating_sub(remaining_fuel),
                memory_bytes,
                host_calls,
            },
        })
    }

    /// Refresh task-scoped managed-window metadata without granting the guest
    /// access to compositor objects or observation bytes.
    pub fn set_computer_context(&mut self, windows: &Value, active_window: &Value) -> Result<()> {
        let windows = serde_json::to_string(windows).context("encode script window context")?;
        let active_window =
            serde_json::to_string(active_window).context("encode active script window")?;
        let source = format!("__broodSetContext({windows}, {active_window})");
        let execution = self.execute(&source)?;
        ensure!(
            execution.plan.calls.is_empty(),
            "window context update unexpectedly emitted host calls"
        );
        ensure!(
            execution.plan.exception.is_none(),
            "window context update failed: {}",
            execution.plan.exception.as_deref().unwrap_or_default()
        );
        Ok(())
    }
}
