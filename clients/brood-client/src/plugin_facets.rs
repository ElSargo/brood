//! Per-actor coordination for optional pure-renderer component instances.
//!
//! This module owns CPU-side admission only. Native graphics preparation calls
//! [`PreparedFacetRender::commit_with`]; a successful callback is the sole path
//! which promotes a speculative frame to input authority.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use brood_protocol::{
    facet_codec::DecodeLimits,
    facets::{
        InputEvent, InputResponse, InputToken, InvocationLimits, PaneTarget, RenderRequest,
        RenderToken, Scope,
    },
};
use brood_wasm::{
    facet_executor::{FacetExecutor, FacetWorker},
    facet_frames::FacetFrames,
    facet_graphics::ValidatedFacetGraphics,
    facet_input::{AdmittedInputBatch, FacetInput, FacetInputConfig},
    facet_resources::{FacetResourceLease, FacetResourceRegistry},
    facet_runtime::{FacetOperation, FacetRuntimeConfig},
    facet_wire,
    snapshot_budget::{BudgetedBytes, BudgetedValue, ByteBudget},
    snapshot_store::SnapshotLease,
};
use tokio::sync::oneshot;

#[cfg(test)]
#[path = "plugin_facets_tests.rs"]
mod tests;

/// Retained committed frames temporarily occupy the bounded resource pool.
/// Declining a host opportunity must leave this healthy facet eligible to retry.
#[derive(Debug)]
pub struct RenderAdmissionDeferred;

impl std::fmt::Display for RenderAdmissionDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("facet resource admission is temporarily full")
    }
}

impl std::error::Error for RenderAdmissionDeferred {}

#[derive(Clone)]
pub struct PluginFacetConfig {
    pub scope: Scope,
    pub runtime: FacetRuntimeConfig,
    pub limits: InvocationLimits,
    pub max_panes: usize,
    pub wire_bytes: usize,
    pub decode: DecodeLimits,
    pub input: FacetInputConfig,
}

#[derive(Clone)]
pub struct RenderParameters {
    pub targets: Vec<PaneTarget>,
    pub candidate: u64,
    pub target_time_ns: u64,
    pub cutoff_ns: u64,
    pub configuration: Vec<u8>,
    pub pane_membership_revision: u64,
    pub geometry_revision: u64,
    pub host_configuration_revision: u64,
    pub renderer_generation: u64,
    pub interaction_revision: u64,
    pub test_delay_completion: bool,
}

enum Completion {
    Render(Result<BudgetedBytes>),
    Input(Result<BudgetedBytes>),
}

pub enum FacetPoll {
    Idle,
    InputCommitted {
        input_sequence: u64,
        sequence_changed: bool,
    },
    InputAdmissionPending,
    RenderReady(PreparedFacetRender),
    DiscardedRender,
    NativeDiscarded,
}

/// CPU-validated output which remains speculative until native preparation.
#[derive(Clone)]
pub struct PreparedFacetRender {
    inner: Arc<PreparedFacetRenderInner>,
}

impl std::fmt::Debug for PreparedFacetRender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedFacetRender")
            .field("strong_count", &Arc::strong_count(&self.inner))
            .finish_non_exhaustive()
    }
}

struct PreparedFacetRenderInner {
    validated: Mutex<Option<ValidatedFacetGraphics>>,
    frames: Arc<Mutex<FacetFrames>>,
    discard_wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Drop for PreparedFacetRenderInner {
    fn drop(&mut self) {
        // Native preparation may discard an already accepted candidate with
        // no subsequent application event. Keep that demand alive. A committed
        // receipt has consumed `validated` and must not schedule a retry.
        if self.validated.get_mut().is_ok_and(|value| value.is_some())
            && let Some(wake) = &self.discard_wake
        {
            wake();
        }
    }
}

impl PreparedFacetRender {
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the retained graphics value is protected for the complete inspection callback"
    )]
    pub fn with_graphics<R>(
        &self,
        inspect: impl FnOnce(&ValidatedFacetGraphics) -> R,
    ) -> Result<R> {
        let validated = self
            .inner
            .validated
            .lock()
            .map_err(|poison| anyhow::anyhow!("facet render receipt poisoned: {poison}"))?;
        let validated = validated
            .as_ref()
            .context("facet render receipt already committed")?;
        Ok(inspect(validated))
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the receipt and frame locks jointly protect one preflight"
    )]
    pub fn validate_current(&self) -> Result<()> {
        let validated = self
            .inner
            .validated
            .lock()
            .map_err(|poison| anyhow::anyhow!("facet render receipt poisoned: {poison}"))?;
        let validated = validated
            .as_ref()
            .context("facet render receipt already committed")?;
        let frames = self
            .inner
            .frames
            .lock()
            .map_err(|poison| anyhow::anyhow!("facet frame coordinator poisoned: {poison}"))?;
        validated.validate_commit(&frames)
    }

    pub fn commit(&self) -> Result<Option<u64>> {
        let mut validated = self
            .inner
            .validated
            .lock()
            .map_err(|poison| anyhow::anyhow!("facet render receipt poisoned: {poison}"))?;
        let value = validated
            .as_ref()
            .context("facet render receipt already committed")?;
        let mut frames = self
            .inner
            .frames
            .lock()
            .map_err(|poison| anyhow::anyhow!("facet frame coordinator poisoned: {poison}"))?;
        value.validate_commit(&frames)?;
        validated
            .take()
            .context("facet render receipt already committed")?
            .commit(&mut frames)
    }
}

/// One coordinator and two isolated contexts belong to one actor generation.
pub struct PluginFacetCoordinator {
    render_worker: FacetWorker,
    input_worker: FacetWorker,
    frames: Arc<Mutex<FacetFrames>>,
    input: FacetInput,
    resources: Arc<FacetResourceRegistry>,
    budget: ByteBudget,
    resource_budget: ByteBudget,
    config: PluginFacetConfig,
    completions: std::sync::mpsc::Receiver<Completion>,
    completion_sender: std::sync::mpsc::SyncSender<Completion>,
    render_inflight: bool,
    input_inflight: bool,
    render_lease: Option<Arc<FacetResourceLease>>,
    task_runtime: tokio::runtime::Handle,
    render_discarded: bool,
    native_discarded: Arc<AtomicU64>,
    render_wake: Option<Arc<dyn Fn() + Send + Sync>>,
    pending_input_response: Option<BudgetedValue<InputResponse>>,
    pending_input_token: Option<InputToken>,
    test_render_completion_delay: Option<Duration>,
}

impl PluginFacetCoordinator {
    /// Load both isolated stores before the owning actor begins lifecycle work.
    pub async fn load(
        executor: &FacetExecutor,
        component: PathBuf,
        config: PluginFacetConfig,
        budget: ByteBudget,
        resource_budget: ByteBudget,
        resources: Arc<FacetResourceRegistry>,
    ) -> Result<Option<Self>> {
        ensure!(budget.generation() == config.scope.actor_generation);
        ensure!(resource_budget.generation() == config.scope.actor_generation);
        let required_buffer = config
            .runtime
            .max_memory_bytes
            .max(config.wire_bytes)
            .max(config.limits.max_output_bytes as usize);
        let budget_limits = budget.limits();
        ensure!(
            required_buffer <= budget_limits.max_buffer_bytes,
            "facet requires a {required_buffer}-byte buffer but its byte budget permits {} bytes",
            budget_limits.max_buffer_bytes
        );
        let concurrent_live = config
            .runtime
            .max_memory_bytes
            .checked_add(config.wire_bytes)
            .and_then(|bytes| bytes.checked_add(config.decode.max_allocated_bytes))
            .and_then(|bytes| bytes.checked_mul(2))
            .ok_or_else(|| anyhow::anyhow!("facet concurrent byte ceiling overflow"))?;
        ensure!(
            concurrent_live <= budget_limits.max_live_bytes,
            "two facet contexts require {concurrent_live} live bytes but their budget permits {}",
            budget_limits.max_live_bytes
        );
        let Some(render_worker) = executor.load(component.clone(), config.runtime).await? else {
            return Ok(None);
        };
        let input_worker = executor
            .load(component, config.runtime)
            .await?
            .context("facet render export exists without an input context")?;
        let frames = FacetFrames::new(
            config.scope.clone(),
            budget.clone(),
            config.max_panes,
            config.limits,
        )?;
        let input = FacetInput::new(config.scope.clone(), budget.clone(), config.input)?;
        let (completion_sender, completions) = std::sync::mpsc::sync_channel(2);
        let task_runtime = tokio::runtime::Handle::current();
        Ok(Some(Self {
            render_worker,
            input_worker,
            frames: Arc::new(Mutex::new(frames)),
            input,
            resources,
            budget,
            resource_budget,
            config,
            completions,
            completion_sender,
            render_inflight: false,
            input_inflight: false,
            render_lease: None,
            task_runtime,
            render_discarded: false,
            native_discarded: Arc::new(AtomicU64::new(0)),
            render_wake: None,
            pending_input_response: None,
            pending_input_token: None,
            test_render_completion_delay: None,
        }))
    }

    /// Delay render completion delivery for deterministic integration tests.
    /// Guest execution still runs immediately and actor progress is unaffected.
    pub fn set_test_render_completion_delay(&mut self, delay: Option<Duration>) {
        self.test_render_completion_delay = delay.filter(|delay| !delay.is_zero());
    }

    /// Start a render without waiting. The snapshot remains pinned through
    /// validation and native commit, and its chunks are copied only into
    /// pre-admitted allocations.
    pub fn try_start_render(
        &mut self,
        snapshot: SnapshotLease,
        parameters: RenderParameters,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<()> {
        ensure!(!self.render_inflight, "facet render already in flight");
        let test_delay_completion = parameters.test_delay_completion
            || std::env::var_os("BROOD_TEST_FACET_DELAY_ALL_RENDERS").is_some();
        let application = copy_snapshot(&snapshot, &self.budget)?;
        let token = self
            .frames
            .lock()
            .map_err(|poison| anyhow::anyhow!("facet frame coordinator poisoned: {poison}"))?
            .issue(
                RenderToken {
                    scope: self.config.scope.clone(),
                    invocation: 0,
                    snapshot_revision: snapshot.revision(),
                    candidate: parameters.candidate,
                    pane_membership_revision: parameters.pane_membership_revision,
                    geometry_revision: parameters.geometry_revision,
                    host_configuration_revision: parameters.host_configuration_revision,
                    renderer_generation: parameters.renderer_generation,
                    interaction_revision: parameters.interaction_revision,
                },
                parameters.targets.clone(),
                snapshot,
            )?;
        let application_bytes = application.bytes()?;
        let sequence = self.input.sequence_state()?;
        let request_bytes = application_bytes
            .len()
            .checked_add(parameters.configuration.len())
            .and_then(|bytes| bytes.checked_add(sequence.len()))
            .and_then(|bytes| {
                bytes.checked_add(
                    parameters
                        .targets
                        .len()
                        .saturating_mul(std::mem::size_of::<PaneTarget>()),
                )
            })
            .ok_or_else(|| anyhow::anyhow!("facet render request length overflow"))?;
        let request = BudgetedValue::capture(&self.budget, request_bytes, || {
            Ok::<_, std::convert::Infallible>(RenderRequest {
                token: token.clone(),
                panes: parameters.targets,
                target_time_ns: parameters.target_time_ns,
                cutoff_ns: parameters.cutoff_ns,
                configuration: parameters.configuration,
                application_view: application_bytes.to_vec(),
                sequence_state: sequence.to_vec(),
                limits: self.config.limits,
            })
        })?;
        let wire =
            facet_wire::render_request(request.value()?, &self.budget, self.config.wire_bytes)?;
        let panes = request
            .value()?
            .panes
            .iter()
            .map(|pane| pane.pane)
            .collect::<Vec<_>>();
        let lease = match self.resources.begin(
            token,
            &panes,
            self.config.limits,
            self.resource_budget.clone(),
        ) {
            Ok(lease) => lease,
            Err(brood_wasm::presentation::PresentationError::LimitReached) => {
                self.discard_pending_render()?;
                return Err(RenderAdmissionDeferred.into());
            }
            Err(error) => return Err(error.into()),
        };
        let receiver = self.render_worker.try_invoke_with_resources(
            wire,
            self.budget.clone(),
            Arc::clone(&lease),
        )?;
        self.render_lease = Some(lease);
        self.render_inflight = true;
        self.render_discarded = false;
        self.render_wake = Some(Arc::clone(&wake));
        forward(
            &self.task_runtime,
            receiver,
            self.completion_sender.clone(),
            wake,
            Completion::Render,
            test_delay_completion
                .then_some(self.test_render_completion_delay)
                .flatten(),
        );
        Ok(())
    }

    /// Pin the currently committed frame at event admission.
    pub fn admit_input(&mut self, pane: u64, event: &InputEvent) -> Result<()> {
        let frame = self
            .frames
            .lock()
            .map_err(|poison| anyhow::anyhow!("facet frame coordinator poisoned: {poison}"))?
            .pin_input(pane)?;
        self.input.admit(frame, event)
    }

    pub fn try_start_input(&mut self, wake: Arc<dyn Fn() + Send + Sync>) -> Result<bool> {
        ensure!(!self.input_inflight, "facet input already in flight");
        ensure!(
            self.pending_input_response.is_none(),
            "completed facet input awaits host admission"
        );
        if !self.input.can_accept_result() {
            return Ok(false);
        }
        let Some(request) = self.input.request()? else {
            return Ok(false);
        };
        let request_token = request.value()?.token.clone();
        let wire =
            match facet_wire::input_request(request.value()?, &self.budget, self.config.wire_bytes)
            {
                Ok(wire) => wire,
                Err(error) => {
                    self.input.retry_pending_request()?;
                    return Err(error);
                }
            };
        let receiver =
            match self
                .input_worker
                .try_invoke(FacetOperation::Interpret, wire, self.budget.clone())
            {
                Ok(receiver) => receiver,
                Err(error) => {
                    self.input.retry_pending_request()?;
                    return Err(error);
                }
            };
        self.input_inflight = true;
        self.pending_input_token = Some(request_token);
        forward(
            &self.task_runtime,
            receiver,
            self.completion_sender.clone(),
            wake,
            Completion::Input,
            None,
        );
        Ok(true)
    }

    /// Poll without blocking the client event loop.
    pub fn poll_completion(&mut self) -> Result<FacetPoll> {
        let discarded_invocation = self.native_discarded.swap(0, Ordering::AcqRel);
        if discarded_invocation != 0 {
            self.frames
                .lock()
                .map_err(|poison| anyhow::anyhow!("facet frames poisoned: {poison}"))?
                .discard_invocation(discarded_invocation);
            return Ok(FacetPoll::NativeDiscarded);
        }
        if let Some(response) = self.pending_input_response.as_ref() {
            let input_sequence = response.value()?.token.input_sequence;
            let sequence_changed =
                self.input.sequence_state()? != response.value()?.next_sequence_state;
            match self.input.commit(response.value()?) {
                Ok(()) => {
                    self.pending_input_response = None;
                    self.pending_input_token = None;
                    return Ok(FacetPoll::InputCommitted {
                        input_sequence,
                        sequence_changed,
                    });
                }
                Err(_) => return Ok(FacetPoll::InputAdmissionPending),
            }
        }
        let Ok(completion) = self.completions.try_recv() else {
            return Ok(FacetPoll::Idle);
        };
        match completion {
            Completion::Render(result) => {
                self.render_inflight = false;
                let lease = self.render_lease.take().context("render lease missing")?;
                if self.render_discarded {
                    drop((lease, result));
                    self.render_discarded = false;
                    return Ok(FacetPoll::DiscardedRender);
                }
                let output = result?;
                let decoded =
                    facet_wire::render_response(&output, &self.budget, self.config.decode)?;
                let frames = self.frames.lock().map_err(|poison| {
                    anyhow::anyhow!("facet frame coordinator poisoned: {poison}")
                })?;
                let validated = ValidatedFacetGraphics::validate(
                    &frames,
                    decoded.value()?,
                    Some(lease),
                    &self.budget,
                )?;
                drop(frames);
                let discarded = Arc::clone(&self.native_discarded);
                let invocation = validated.token().invocation;
                let wake = self
                    .render_wake
                    .take()
                    .context("render completion wake missing")?;
                Ok(FacetPoll::RenderReady(PreparedFacetRender {
                    inner: Arc::new(PreparedFacetRenderInner {
                        validated: Mutex::new(Some(validated)),
                        frames: Arc::clone(&self.frames),
                        discard_wake: Some(Arc::new(move || {
                            if discarded.fetch_max(invocation, Ordering::AcqRel) == 0 {
                                wake();
                            }
                        })),
                    }),
                }))
            }
            Completion::Input(result) => {
                self.input_inflight = false;
                let response = match result.and_then(|output| {
                    facet_wire::input_response(&output, &self.budget, self.config.decode)
                }) {
                    Ok(response) => response,
                    Err(error) => {
                        self.input.retry_pending_request()?;
                        self.pending_input_token = None;
                        return Err(error);
                    }
                };
                validate_input_response(
                    response.value()?,
                    self.pending_input_token
                        .as_ref()
                        .context("facet input token missing")?,
                    self.config.limits,
                )?;
                let sequence_changed =
                    self.input.sequence_state()? != response.value()?.next_sequence_state;
                let input_sequence = response.value()?.token.input_sequence;
                if self.input.commit(response.value()?).is_ok() {
                    self.pending_input_token = None;
                    Ok(FacetPoll::InputCommitted {
                        input_sequence,
                        sequence_changed,
                    })
                } else {
                    self.pending_input_response = Some(response);
                    Ok(FacetPoll::InputAdmissionPending)
                }
            }
        }
    }

    /// Acknowledge semantic delivery only after the actor transport accepted
    /// the complete batch and its own mailbox reservation.
    pub fn try_deliver(
        &mut self,
        mut send: impl FnMut(&AdmittedInputBatch) -> Result<()>,
    ) -> Result<bool> {
        let Some(batch) = self.input.delivery() else {
            return Ok(false);
        };
        let token = batch.token.clone();
        send(batch)?;
        self.input.acknowledge_delivery(&token)?;
        Ok(true)
    }

    pub const fn is_render_inflight(&self) -> bool {
        self.render_inflight
    }

    pub fn awaits_native_commit(&self) -> bool {
        self.frames.lock().is_ok_and(|frames| frames.has_pending())
    }

    pub const fn is_input_inflight(&self) -> bool {
        self.input_inflight
    }

    pub const fn budget(&self) -> &ByteBudget {
        &self.budget
    }

    /// Supersede CPU/native speculative render work while preserving committed
    /// input authority and its reliable delivery queue.
    pub fn discard_pending_render(&mut self) -> Result<()> {
        self.frames
            .lock()
            .map_err(|poison| anyhow::anyhow!("facet frame coordinator poisoned: {poison}"))?
            .discard_pending();
        self.render_discarded |= self.render_inflight;
        if !self.render_inflight {
            self.render_lease = None;
        }
        Ok(())
    }

    /// Permanently revoke this generation. The owner drops the coordinator to
    /// stop both isolated workers after any guest/runtime failure.
    pub fn invalidate(&mut self) {
        let _cancelled = self.input.cancel();
        if let Ok(mut frames) = self.frames.lock() {
            frames.invalidate();
        }
        self.budget.invalidate();
        self.resource_budget.invalidate();
        self.render_lease = None;
        self.render_inflight = false;
        self.input_inflight = false;
        self.pending_input_response = None;
        self.pending_input_token = None;
    }
}

fn validate_input_response(
    response: &InputResponse,
    token: &InputToken,
    limits: InvocationLimits,
) -> Result<()> {
    ensure!(
        response.token == *token,
        "stale or foreign facet input response"
    );
    ensure!(
        response.next_sequence_state.len() <= limits.max_sequence_bytes as usize,
        "facet input sequence result exceeds its limit"
    );
    ensure!(
        response.events.len() <= limits.max_events as usize
            && response
                .events
                .iter()
                .all(|event| event.len() <= limits.max_event_bytes as usize),
        "facet semantic result exceeds its limits"
    );
    ensure!(
        response.requests.len() <= limits.max_host_requests as usize,
        "facet host request result exceeds its limit"
    );
    Ok(())
}

fn copy_snapshot(snapshot: &SnapshotLease, budget: &ByteBudget) -> Result<BudgetedBytes> {
    ensure!(snapshot.is_authoritative(), "snapshot is invalidated");
    Ok(BudgetedBytes::capture_bounded(
        budget,
        snapshot.logical_bytes(),
        || {
            let mut bytes = Vec::with_capacity(snapshot.logical_bytes());
            for chunk in snapshot.iter_chunks() {
                bytes.extend_from_slice(chunk);
            }
            Ok::<_, std::convert::Infallible>(bytes)
        },
    )?)
}

fn forward(
    runtime: &tokio::runtime::Handle,
    receiver: oneshot::Receiver<Result<BudgetedBytes>>,
    sender: std::sync::mpsc::SyncSender<Completion>,
    wake: Arc<dyn Fn() + Send + Sync>,
    wrap: fn(Result<BudgetedBytes>) -> Completion,
    completion_delay: Option<Duration>,
) {
    runtime.spawn(async move {
        let result = receiver
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("facet worker dropped completion")));
        if let Some(delay) = completion_delay {
            tokio::time::sleep(delay).await;
        }
        if sender.try_send(wrap(result)).is_ok() {
            wake();
        }
    });
}
