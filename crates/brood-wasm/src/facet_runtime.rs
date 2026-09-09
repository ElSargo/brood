//! Isolated Component Model runtime for optional pure plugin facets.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use wasmtime::{
    AsContext, Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{
        Component, ComponentExportIndex, Func, Instance, Linker, WasmList, WasmStr,
        types::ComponentItem,
    },
};

use crate::{
    facet_linker::link_imports_with,
    facet_resources::FacetResourceLease,
    presentation::{FontIntent, FontStyle, FontWeight, PresentationError, TextLayoutOptions},
    snapshot_budget::{BudgetedBytes, ByteBudget, CaptureBoundedError},
};

pub const FACETS_INTERFACE: &str = "brood:plugin/facets@0.1.0";
const HOST_INTERFACE: &str = "brood:plugin/host@0.1.0";

#[derive(
    Clone,
    Copy,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(enum)]
#[repr(u8)]
enum GraphicsError {
    #[component(name = "unavailable")]
    Unavailable,
    #[component(name = "invalid-pane")]
    InvalidPane,
    #[component(name = "invalid-token")]
    InvalidToken,
    #[component(name = "duplicate-pane")]
    DuplicatePane,
    #[component(name = "stale-resize-epoch")]
    StaleResizeEpoch,
    #[component(name = "invalid-argument")]
    InvalidArgument,
    #[component(name = "limit-reached")]
    LimitReached,
    #[component(name = "invalid-packet")]
    InvalidPacket,
    #[component(name = "invalid-resource")]
    InvalidResource,
    #[component(name = "resource-in-use")]
    ResourceInUse,
    #[component(name = "host-failure")]
    HostFailure,
}

#[derive(
    Clone,
    Copy,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(enum)]
#[repr(u8)]
#[allow(
    dead_code,
    reason = "variants are constructed by canonical ABI lifting"
)]
enum AbiFontIntent {
    #[component(name = "ui")]
    Ui,
    #[component(name = "monospace")]
    Monospace,
}
#[derive(
    Clone,
    Copy,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(enum)]
#[repr(u8)]
#[allow(
    dead_code,
    reason = "variants are constructed by canonical ABI lifting"
)]
enum AbiFontWeight {
    #[component(name = "normal")]
    Normal,
    #[component(name = "bold")]
    Bold,
}
#[derive(
    Clone,
    Copy,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(enum)]
#[repr(u8)]
#[allow(
    dead_code,
    reason = "variants are constructed by canonical ABI lifting"
)]
enum AbiFontStyle {
    #[component(name = "normal")]
    Normal,
    #[component(name = "italic")]
    Italic,
}

#[derive(
    Clone,
    Copy,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(record)]
struct AbiTextLayoutOptions {
    #[component(name = "font-intent")]
    font_intent: AbiFontIntent,
    #[component(name = "font-weight")]
    font_weight: AbiFontWeight,
    #[component(name = "font-style")]
    font_style: AbiFontStyle,
    #[component(name = "font-size")]
    font_size: f32,
    #[component(name = "max-width")]
    max_width: f32,
    #[component(name = "line-height")]
    line_height: f32,
    red: f32,
    green: f32,
    blue: f32,
    alpha: f32,
}

#[derive(
    Clone,
    Copy,
    Debug,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(record)]
struct AbiTextLayoutResult {
    handle: u64,
    width: f32,
    height: f32,
    #[component(name = "first-baseline")]
    first_baseline: f32,
    #[component(name = "line-count")]
    line_count: u32,
    #[component(name = "cell-width")]
    cell_width: f32,
    #[component(name = "line-height")]
    line_height: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FacetOperation {
    Render,
    Interpret,
}

#[derive(Clone, Copy, Debug)]
pub struct FacetRuntimeConfig {
    pub fuel: u64,
    pub epoch_tick_interval: Duration,
    pub epoch_deadline_ticks: u64,
    pub max_memory_bytes: usize,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
}

impl Default for FacetRuntimeConfig {
    fn default() -> Self {
        Self {
            fuel: 25_000_000,
            epoch_tick_interval: Duration::from_millis(2),
            epoch_deadline_ticks: 50,
            max_memory_bytes: 4 * 1024 * 1024,
            max_input_bytes: 4 * 1024 * 1024,
            max_output_bytes: 64 * 1024,
        }
    }
}

/// A synchronous facet instance intended to live on one dedicated host worker.
///
/// Do not invoke it from an existing asynchronous executor thread: calls enter
/// its private current-thread Tokio runtime until the guest returns or traps.
pub struct FacetRuntime {
    context: Option<FacetContext>,
    runtime: tokio::runtime::Runtime,
    config: FacetRuntimeConfig,
    usable: bool,
    _epochs: EpochDriver,
}

struct FacetContext {
    store: Store<FacetStore>,
    render: Func,
    interpret: Func,
}

struct FacetStore {
    limits: StoreLimits,
    resources: Option<Arc<FacetResourceLease>>,
}

fn define_graphics_import(
    linker: &mut wasmtime::component::LinkerInstance<'_, FacetStore>,
    name: &str,
    path: &str,
) -> Result<bool> {
    match path {
        "brood:plugin/host@0.1.0.graphics-create-image-rgba8" => linker.func_wrap(
            name,
            |caller, (pane, width, height, pixels): (u64, u32, u32, WasmList<u8>)| {
                let result = caller
                    .data()
                    .resources
                    .as_ref()
                    .ok_or(GraphicsError::Unavailable)
                    .and_then(|lease| {
                        lease
                            .create_image(
                                pane,
                                width,
                                height,
                                pixels.as_le_slice(caller.as_context()),
                            )
                            .map_err(map_graphics_error)
                    });
                Ok((result,))
            },
        )?,
        "brood:plugin/host@0.1.0.graphics-layout-text" => linker.func_wrap(
            name,
            |caller, (pane, text, options): (u64, WasmStr, AbiTextLayoutOptions)| {
                let result = text
                    .to_str(caller.as_context())
                    .map_err(|_encoding_error| GraphicsError::InvalidArgument)
                    .and_then(|text| {
                        caller
                            .data()
                            .resources
                            .as_ref()
                            .ok_or(GraphicsError::Unavailable)
                            .and_then(|lease| {
                                lease
                                    .create_text(pane, text.as_ref(), options.into())
                                    .map(AbiTextLayoutResult::from)
                                    .map_err(map_graphics_error)
                            })
                    });
                Ok((result,))
            },
        )?,
        "brood:plugin/host@0.1.0.graphics-drop-resource" => {
            linker.func_wrap(name, |caller, (pane, handle): (u64, u64)| {
                let result = caller
                    .data()
                    .resources
                    .as_ref()
                    .ok_or(GraphicsError::Unavailable)
                    .and_then(|lease| {
                        lease
                            .drop_resource(pane, handle)
                            .map_err(map_graphics_error)
                    });
                Ok((result,))
            })?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

const fn map_graphics_error(error: PresentationError) -> GraphicsError {
    match error {
        PresentationError::Unavailable | PresentationError::ActorStopped => {
            GraphicsError::Unavailable
        }
        PresentationError::InvalidPane => GraphicsError::InvalidPane,
        PresentationError::InvalidToken => GraphicsError::InvalidToken,
        PresentationError::DuplicatePane => GraphicsError::DuplicatePane,
        PresentationError::StaleResizeEpoch => GraphicsError::StaleResizeEpoch,
        PresentationError::InvalidArgument => GraphicsError::InvalidArgument,
        PresentationError::LimitReached => GraphicsError::LimitReached,
        PresentationError::InvalidPacket => GraphicsError::InvalidPacket,
        PresentationError::InvalidResource => GraphicsError::InvalidResource,
        PresentationError::ResourceInUse => GraphicsError::ResourceInUse,
        PresentationError::HostFailure => GraphicsError::HostFailure,
    }
}

impl From<AbiTextLayoutOptions> for TextLayoutOptions {
    fn from(value: AbiTextLayoutOptions) -> Self {
        Self {
            font_intent: match value.font_intent {
                AbiFontIntent::Ui => FontIntent::Ui,
                AbiFontIntent::Monospace => FontIntent::Monospace,
            },
            font_weight: match value.font_weight {
                AbiFontWeight::Normal => FontWeight::Normal,
                AbiFontWeight::Bold => FontWeight::Bold,
            },
            font_style: match value.font_style {
                AbiFontStyle::Normal => FontStyle::Normal,
                AbiFontStyle::Italic => FontStyle::Italic,
            },
            font_size: value.font_size,
            max_width: value.max_width,
            line_height: value.line_height,
            color: [value.red, value.green, value.blue, value.alpha],
        }
    }
}

impl From<crate::presentation::TextLayoutResult> for AbiTextLayoutResult {
    fn from(value: crate::presentation::TextLayoutResult) -> Self {
        Self {
            handle: value.handle,
            width: value.width,
            height: value.height,
            first_baseline: value.first_baseline,
            line_count: value.line_count,
            cell_width: value.cell_width,
            line_height: value.line_height,
        }
    }
}

impl FacetRuntime {
    pub fn load(path: impl AsRef<Path>, limits: &FacetRuntimeConfig) -> Result<Option<Self>> {
        validate_config(limits)?;
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .wasm_component_model_async(true)
            .concurrency_support(true)
            .consume_fuel(true)
            .epoch_interruption(true);
        let engine = Engine::new(&config)?;
        let component = Component::from_file(&engine, path)?;
        let Some(interface) = facet_interface_type(&engine, &component)? else {
            return Ok(None);
        };
        let mut linker = Linker::new(&engine);
        link_imports_with(
            &mut linker,
            &component,
            |instance, name, path, asynchronous| {
                if path.starts_with(&format!("{HOST_INTERFACE}.graphics-")) {
                    ensure!(!asynchronous, "graphics facet import unexpectedly async");
                    return define_graphics_import(instance, name, path);
                }
                Ok(false)
            },
        )?;
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes)
            .memories(1)
            .instances(4)
            .tables(2)
            .table_elements(256)
            .build();
        let mut store = Store::new(
            &engine,
            FacetStore {
                limits: store_limits,
                resources: None,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(limits.fuel)?;
        store.set_epoch_deadline(limits.epoch_deadline_ticks);
        store.epoch_deadline_trap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        let epochs = EpochDriver::start(engine, limits.epoch_tick_interval);
        let instance = runtime.block_on(linker.instantiate_async(&mut store, &component))?;
        let render = facet_func(&instance, &mut store, &interface, "render")?;
        let interpret = facet_func(&instance, &mut store, &interface, "interpret")?;
        Ok(Some(Self {
            context: Some(FacetContext {
                store,
                render,
                interpret,
            }),
            runtime,
            config: *limits,
            usable: true,
            _epochs: epochs,
        }))
    }

    #[must_use]
    pub const fn is_usable(&self) -> bool {
        self.usable
    }

    pub fn invoke(
        &mut self,
        operation: FacetOperation,
        input: &[u8],
        budget: &ByteBudget,
    ) -> Result<BudgetedBytes> {
        self.invoke_inner(operation, input, budget, None)
    }

    pub fn invoke_with_resources(
        &mut self,
        input: &[u8],
        budget: &ByteBudget,
        resources: Arc<FacetResourceLease>,
    ) -> Result<BudgetedBytes> {
        self.invoke_inner(FacetOperation::Render, input, budget, Some(resources))
    }

    fn invoke_inner(
        &mut self,
        operation: FacetOperation,
        input: &[u8],
        budget: &ByteBudget,
        resources: Option<Arc<FacetResourceLease>>,
    ) -> Result<BudgetedBytes> {
        let _profile_span = brood_profiler::Span::new(
            "wasm",
            match operation {
                FacetOperation::Render => "facet_render",
                FacetOperation::Interpret => "facet_interpret",
            },
            None,
            Vec::new(),
        );
        ensure!(
            self.usable,
            "facet runtime is unavailable; reload is required"
        );
        ensure!(
            input.len() <= self.config.max_input_bytes,
            "facet input exceeds configured limit"
        );
        let context = self.context.as_mut().context("facet context was dropped")?;
        if let Err(error) = context.store.set_fuel(self.config.fuel) {
            self.invalidate();
            return Err(anyhow::anyhow!("reset facet fuel: {error}"));
        }
        context
            .store
            .set_epoch_deadline(self.config.epoch_deadline_ticks);
        let function = match operation {
            FacetOperation::Render => &context.render,
            FacetOperation::Interpret => &context.interpret,
        }
        .typed::<(&[u8],), (Vec<u8>,)>(&context.store)
        .map_err(|error| anyhow::anyhow!("validated facet signature changed: {error}"))?;
        context.store.data_mut().resources = resources;
        let result = BudgetedBytes::capture_bounded(budget, self.config.max_memory_bytes, || {
            self.runtime
                .block_on(function.call_async(&mut context.store, (input,)))
                .map(|(bytes,)| bytes)
        });
        let resources = context.store.data_mut().resources.take();
        match result {
            Ok(output) => {
                if let Err(error) = seal_resources(resources.as_ref()) {
                    self.invalidate();
                    return Err(error);
                }
                let len = match output.bytes() {
                    Ok(bytes) => bytes.len(),
                    Err(error) => {
                        self.invalidate();
                        return Err(error.into());
                    }
                };
                if len > self.config.max_output_bytes {
                    self.invalidate();
                    bail!(
                        "facet output has {len} bytes, exceeding configured {}-byte limit",
                        self.config.max_output_bytes
                    );
                }
                Ok(output)
            }
            Err(CaptureBoundedError::Budget(error)) => Err(error.into()),
            Err(error) => {
                if let Err(seal_error) = seal_resources(resources.as_ref()) {
                    self.invalidate();
                    return Err(seal_error);
                }
                self.invalidate();
                Err(match error {
                    CaptureBoundedError::Producer(error) => {
                        error.context("facet invocation failed").into()
                    }
                    error => anyhow::Error::new(error).context("facet invocation failed"),
                })
            }
        }
    }

    fn invalidate(&mut self) {
        self.usable = false;
        self.context.take();
    }
}

fn seal_resources(resources: Option<&Arc<FacetResourceLease>>) -> Result<()> {
    if let Some(resources) = resources {
        resources
            .seal()
            .map_err(|error| anyhow::anyhow!("seal facet resources: {error}"))?;
    }
    Ok(())
}

fn validate_config(config: &FacetRuntimeConfig) -> Result<()> {
    ensure!(config.fuel > 0, "facet fuel must be nonzero");
    ensure!(
        !config.epoch_tick_interval.is_zero() && config.epoch_deadline_ticks > 0,
        "facet epoch bounds must be nonzero"
    );
    ensure!(
        config.max_memory_bytes > 0
            && config.max_input_bytes <= config.max_memory_bytes
            && config.max_output_bytes <= config.max_memory_bytes,
        "facet byte bounds are inconsistent"
    );
    Ok(())
}

fn facet_interface_type(engine: &Engine, component: &Component) -> Result<Option<String>> {
    for (name, export) in component.component_type().exports(engine) {
        if name == FACETS_INTERFACE {
            ensure!(
                matches!(export.ty, ComponentItem::ComponentInstance(_)),
                "facet interface export has wrong type"
            );
            return Ok(Some(name.to_owned()));
        }
    }
    Ok(None)
}

fn facet_func(
    store_instance: &Instance,
    store: &mut Store<FacetStore>,
    interface_name: &str,
    name: &str,
) -> Result<Func> {
    let (_, interface): (_, ComponentExportIndex) = store_instance
        .get_export(&mut *store, None, interface_name)
        .context("facet interface disappeared after instantiation")?;
    let (_, function) = store_instance
        .get_export(&mut *store, Some(&interface), name)
        .with_context(|| format!("facet interface is missing `{name}`"))?;
    let function = store_instance
        .get_func(&mut *store, function)
        .context("facet export is not a function")?;
    function
        .typed::<(&[u8],), (Vec<u8>,)>(&*store)
        .map_err(|error| anyhow::anyhow!("facet `{name}` has the wrong signature: {error}"))?;
    Ok(function)
}

struct EpochDriver {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl EpochDriver {
    fn start(engine: Engine, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                thread::park_timeout(interval);
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
