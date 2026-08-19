//! Server-owned automation viewport and observation capture.

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::Cursor,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use brood_computer_use::{
    AUTOMATION_VIEWPORT_HEIGHT, AUTOMATION_VIEWPORT_WIDTH, ActionStatus, ComputerAction,
    ComputerActionBatch, ComputerActionResult, ComputerObservation, ComputerObservationFrame,
    ComputerObservationFrameChunk, ComputerObservationSummary, ComputerRect, ComputerScriptMetrics,
    ComputerScriptRequest, ComputerScriptResult, ComputerUseFailure, ComputerUseLimits,
    ComputerUseObserverCommand, ComputerUseObserverUpdate, ComputerWindow,
    MAX_OBSERVATION_PNG_BYTES, MAX_OBSERVER_FRAME_CHUNKS, MAX_QUEUED_OBSERVATIONS,
    MIN_CAPTURE_INTERVAL_MS, OBSERVER_FRAME_CHUNK_BYTES, PointerButton, SettleEvidence, TaskId,
};
use brood_wasm::{ComputerUseJsConfig, ComputerUseJsIsolate, ComputerUseJsMetrics};
use image::{ColorType, ImageEncoder, codecs::png::PngEncoder};
use num_traits::ToPrimitive;
use smithay::{
    backend::{
        allocator::Fourcc,
        input::{ButtonState, KeyState, Keycode},
        renderer::{
            Bind, Offscreen, element::solid::SolidColorRenderElement, gles::GlesRenderbuffer,
        },
    },
    desktop::Space,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    utils::{Logical, Physical, Point, Size, Transform},
};

use crate::{
    buffer_readback,
    frame_export::FrameExportFailure,
    state::{Brood, SESSION_OUTPUT_REFRESH_MILLIHZ},
};

const CAPTURE_INTERVAL: Duration = Duration::from_millis(MIN_CAPTURE_INTERVAL_MS);
const SETTLE_QUIET: Duration = Duration::from_millis(75);
const SETTLE_TIMEOUT: Duration = Duration::from_millis(750);
const KEYPRESS_HOLD: Duration = Duration::from_millis(100);

type BatchResponse = tokio::sync::oneshot::Sender<std::result::Result<String, String>>;

struct PendingBatch {
    execution_id: u64,
    batch: ComputerActionBatch,
    sequence: u64,
    target_pane: u64,
    next_action: usize,
    settle_started: Option<Instant>,
    results: Vec<ComputerActionResult>,
    active_action: Option<ActiveAction>,
    response: BatchResponse,
    adapter_call_id: Option<String>,
    script: Option<PendingScript>,
}

struct PendingScript {
    console: Vec<String>,
    value_json: Option<String>,
    exception: Option<String>,
    metrics: ComputerUseJsMetrics,
    display_latest_observation: bool,
}

struct PendingScriptCheckpoint {
    approval_id: String,
    source: String,
    batch: ComputerActionBatch,
    script: PendingScript,
}

enum ActiveAction {
    Drag {
        path: Vec<brood_computer_use::ComputerPoint>,
        button: PointerButton,
        next_point: usize,
        started_at_ms: u64,
    },
    Keypress {
        keys: Vec<Keycode>,
        started_at_ms: u64,
    },
}

#[derive(Clone, Debug)]
pub struct AutomationSnapshot {
    pub captured_at_ms: u64,
    pub frame_sequence: u64,
    pub active_window: Option<u64>,
    pub windows: Vec<ComputerWindow>,
    pub png: Vec<u8>,
}

#[derive(Default)]
pub struct AutomationRuntime {
    active_windows: BTreeMap<u64, u64>,
    cursor_positions: BTreeMap<u64, (u32, u32)>,
    unlocked_lease_generation: Option<u64>,
    latest: Option<AutomationSnapshot>,
    observations: VecDeque<ComputerObservation>,
    last_capture_at: Option<Instant>,
    last_managed_commit_at: Option<Instant>,
    frame_callback_pending: bool,
    pending_batch: Option<PendingBatch>,
    held_keys: Vec<Keycode>,
    held_buttons: Vec<(u64, u32)>,
    next_execution_id: u64,
    next_frame_sequence: u64,
    next_observation_sequence: u64,
    next_approval_sequence: u64,
    observer_recipient: Option<String>,
    observer_event_cursor: u64,
    observer_observation_sequence: u64,
    script_isolates: BTreeMap<TaskId, ComputerUseJsIsolate>,
    script_checkpoints: BTreeMap<TaskId, PendingScriptCheckpoint>,
    pub capture_attempts: u64,
    pub capture_failures: u64,
    pub captures_dropped_cadence: u64,
    pub observations_dropped_backpressure: u64,
    pub last_error: Option<String>,
}

impl std::fmt::Debug for AutomationRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AutomationRuntime")
            .field("active_windows", &self.active_windows)
            .field("cursor_positions", &self.cursor_positions)
            .field("unlocked_lease_generation", &self.unlocked_lease_generation)
            .field("latest", &self.latest)
            .field("observations", &self.observations.len())
            .field("pending_batch", &self.pending_batch.is_some())
            .field("held_keys", &self.held_keys)
            .field("held_buttons", &self.held_buttons)
            .field("script_isolates", &self.script_isolates.len())
            .field("script_checkpoints", &self.script_checkpoints.len())
            .finish_non_exhaustive()
    }
}

impl AutomationRuntime {
    pub(crate) const fn unlock_lease(&mut self, generation: u64) {
        self.unlocked_lease_generation = Some(generation);
    }

    pub(crate) fn lease_is_unlocked(&self, generation: u64) -> bool {
        self.unlocked_lease_generation == Some(generation)
    }

    pub(crate) fn cursor_position(&self, pane: u64) -> Option<(u32, u32)> {
        self.cursor_positions.get(&pane).copied()
    }

    pub(crate) fn selected_pane(&self, state: &Brood, session: u64) -> Option<u64> {
        let permitted = |pane| {
            state
                .presentation_window_summaries()
                .iter()
                .any(|window| window.session == session && window.pane == pane)
        };
        self.active_windows
            .get(&session)
            .copied()
            .filter(|pane| permitted(*pane))
            .or_else(|| {
                state.active_window().and_then(|window| {
                    let pane = state.window_pane_id(&window)?;
                    (state.window_session_id(&window) == Some(session)).then_some(pane)
                })
            })
            .or_else(|| {
                state
                    .presentation_window_summaries()
                    .into_iter()
                    .filter(|window| window.session == session)
                    .map(|window| window.pane)
                    .min()
            })
    }

    pub fn select_window(&mut self, session: u64, pane: u64) {
        self.active_windows.insert(session, pane);
    }

    fn capture_due(&mut self, now: Instant) -> bool {
        if self
            .last_capture_at
            .is_some_and(|previous| now.duration_since(previous) < CAPTURE_INTERVAL)
        {
            self.captures_dropped_cadence = self.captures_dropped_cadence.saturating_add(1);
            return false;
        }
        self.last_capture_at = Some(now);
        self.capture_attempts = self.capture_attempts.saturating_add(1);
        true
    }

    fn store_capture(&mut self, mut snapshot: AutomationSnapshot) {
        self.next_frame_sequence = self.next_frame_sequence.saturating_add(1).max(1);
        snapshot.frame_sequence = self.next_frame_sequence;
        self.latest = Some(snapshot);
        self.last_error = None;
    }

    fn record_failure(&mut self, error: &FrameExportFailure) {
        self.capture_failures = self.capture_failures.saturating_add(1);
        self.last_error = Some(error.to_string());
    }

    pub(crate) fn observation(
        &mut self,
        task: &TaskId,
        input_sequence: u64,
        settled: bool,
        quiet_ms: u32,
        timed_out: bool,
    ) -> Option<ComputerObservation> {
        let snapshot = self.latest.clone()?;
        self.next_observation_sequence = self.next_observation_sequence.saturating_add(1).max(1);
        let observation = ComputerObservation {
            protocol_version: brood_computer_use::COMPUTER_USE_PROTOCOL_VERSION,
            task: task.clone(),
            sequence: self.next_observation_sequence,
            captured_at_ms: snapshot.captured_at_ms,
            viewport_width: AUTOMATION_VIEWPORT_WIDTH,
            viewport_height: AUTOMATION_VIEWPORT_HEIGHT,
            active_window: snapshot.active_window,
            windows: snapshot.windows,
            png: snapshot.png,
            settle: SettleEvidence {
                settled,
                quiet_ms,
                timed_out,
                frame_sequence: snapshot.frame_sequence,
                input_sequence,
            },
        };
        if self.observations.len() == MAX_QUEUED_OBSERVATIONS {
            self.observations.pop_front();
            self.observations_dropped_backpressure =
                self.observations_dropped_backpressure.saturating_add(1);
        }
        self.observations.push_back(observation.clone());
        Some(observation)
    }

    pub const fn latest(&self) -> Option<&AutomationSnapshot> {
        self.latest.as_ref()
    }

    pub(crate) fn queued_observations(&self) -> usize {
        self.observations.len()
    }

    pub(crate) fn script_isolate_count(&self) -> usize {
        self.script_isolates.len()
    }

    pub(crate) fn script_checkpoint_count(&self) -> usize {
        self.script_checkpoints.len()
    }

    pub(crate) fn cancel_script_checkpoint_by_approval(&mut self, approval_id: &str) -> bool {
        let before = self.script_checkpoints.len();
        self.script_checkpoints
            .retain(|_, checkpoint| checkpoint.approval_id != approval_id);
        self.script_checkpoints.len() != before
    }

    pub(crate) fn observations_after(
        &self,
        task: &TaskId,
        sequence: u64,
    ) -> Vec<ComputerObservation> {
        self.observations
            .iter()
            .filter(|observation| observation.task == *task && observation.sequence > sequence)
            .cloned()
            .collect()
    }

    fn next_approval_id(&mut self) -> String {
        self.next_approval_sequence = self.next_approval_sequence.saturating_add(1).max(1);
        format!("approval-{}", self.next_approval_sequence)
    }

    fn observation_summaries_after(&self, sequence: u64) -> Vec<ComputerObservationSummary> {
        self.observations
            .iter()
            .filter(|observation| observation.sequence > sequence)
            .map(ComputerObservationSummary::from)
            .collect()
    }

    fn latest_observation_after(
        &self,
        sequence: u64,
        task: Option<&TaskId>,
    ) -> Option<ComputerObservation> {
        self.observations
            .iter()
            .rev()
            .find(|observation| {
                observation.sequence > sequence && task.is_none_or(|task| observation.task == *task)
            })
            .cloned()
    }

    pub(crate) const fn pending_batch(&self) -> bool {
        self.pending_batch.is_some()
    }

    pub(crate) const fn held_key_count(&self) -> usize {
        self.held_keys.len()
    }

    pub(crate) const fn held_button_count(&self) -> usize {
        self.held_buttons.len()
    }
}

pub fn handle_observer_command(
    state: &mut Brood,
    sender: String,
    body: &[u8],
) -> Result<(), String> {
    let command = serde_json::from_slice::<ComputerUseObserverCommand>(body)
        .map_err(|error| format!("decode computer-use observer command: {error}"))?;
    match command {
        ComputerUseObserverCommand::Subscribe {
            after_event_cursor,
            after_observation_sequence,
        } => {
            state.computer_use_automation.observer_recipient = Some(sender);
            state.computer_use_automation.observer_event_cursor = after_event_cursor;
            state.computer_use_automation.observer_observation_sequence =
                after_observation_sequence;
        }
        ComputerUseObserverCommand::Pause { task } => {
            cancel_pending_batch(state, &task, "computer-use task paused by observer");
            let now_ms = monotonic_ms(state);
            state
                .computer_use
                .pause(&task, now_ms)
                .map_err(|error| error.to_string())?;
        }
        ComputerUseObserverCommand::Resume {
            task,
            lease_duration_ms,
        } => {
            let now_ms = monotonic_ms(state);
            state
                .computer_use
                .resume(&task, now_ms)
                .map_err(|error| error.to_string())?;
            let lease = state
                .computer_use
                .acquire_lease(&task, now_ms, lease_duration_ms)
                .map_err(|error| error.to_string())?;
            if let Some(pane) = lease.windows.iter().next().copied() {
                activate_window(state, lease.session, pane)?;
            }
        }
        ComputerUseObserverCommand::Unlock { task } => {
            let lease = state
                .computer_use
                .lease()
                .filter(|lease| lease.task == task)
                .cloned()
                .ok_or_else(|| "computer-use task does not hold the active lease".to_string())?;
            state.computer_use_automation.unlock_lease(lease.generation);
            crate::frame_export::request_redraw(state);
        }
        ComputerUseObserverCommand::Stop { task } => {
            cancel_pending_batch(state, &task, "computer-use task stopped by observer");
            drop_script_isolate(state, &task);
            let now_ms = monotonic_ms(state);
            state
                .computer_use
                .stop(&task, now_ms)
                .map_err(|error| error.to_string())?;
        }
        ComputerUseObserverCommand::DecideApproval { decision } => {
            let denied_request = (!decision.approved).then(|| decision.request_id.clone());
            state
                .computer_use
                .decide_approval(&decision)
                .map_err(|error| error.to_string())?;
            if let Some(request_id) = denied_request {
                state
                    .computer_use_automation
                    .script_checkpoints
                    .retain(|_, checkpoint| checkpoint.approval_id != request_id);
            }
        }
    }
    publish_observer_update(state)
}

pub fn publish_observer_update(state: &mut Brood) -> Result<(), String> {
    let Some(recipient) = state.computer_use_automation.observer_recipient.clone() else {
        return Ok(());
    };
    let after_event_cursor = state.computer_use_automation.observer_event_cursor;
    let after_observation_sequence = state.computer_use_automation.observer_observation_sequence;
    let observations = state
        .computer_use_automation
        .observation_summaries_after(after_observation_sequence);
    let leased_task = state.computer_use.lease().map(|lease| lease.task.clone());
    let frame = state
        .computer_use_automation
        .latest_observation_after(after_observation_sequence, leased_task.as_ref())
        .as_ref()
        .map(ComputerObservationFrame::from);
    let update = ComputerUseObserverUpdate {
        protocol_version: brood_computer_use::COMPUTER_USE_PROTOCOL_VERSION,
        event_cursor: state.computer_use.latest_event_cursor(),
        tasks: state.computer_use.tasks(),
        lease: state.computer_use.lease().cloned(),
        approvals: state.computer_use.pending_approvals(None),
        events: state.computer_use.events_after(
            after_event_cursor,
            None,
            brood_computer_use::MAX_EVENT_PAGE,
        ),
        observations,
        frame: None,
    };
    let encoded = serde_json::to_vec(&update).map_err(|error| error.to_string())?;
    if let Err(error) = state.send_computer_use_observer_message(&recipient, &encoded) {
        tracing::warn!(
            recipient,
            encoded_bytes = encoded.len(),
            %error,
            "computer-use observer update could not be queued"
        );
        state.computer_use_automation.observer_recipient = None;
        return Ok(());
    }
    state.computer_use_automation.observer_event_cursor = update.event_cursor;
    if let Some(sequence) = update
        .observations
        .last()
        .map(|observation| observation.sequence)
    {
        state.computer_use_automation.observer_observation_sequence = sequence;
    }

    let Some(frame) = frame else {
        return Ok(());
    };
    let frame_bytes = serde_json::to_vec(&frame).map_err(|error| error.to_string())?;
    let chunk_count = frame_bytes.len().div_ceil(OBSERVER_FRAME_CHUNK_BYTES);
    if chunk_count > usize::from(MAX_OBSERVER_FRAME_CHUNKS) {
        tracing::warn!(
            recipient,
            frame_bytes = frame_bytes.len(),
            max_chunks = MAX_OBSERVER_FRAME_CHUNKS,
            "computer-use spectator frame exceeds the bounded observer transport"
        );
        return Ok(());
    }
    let chunk_count = u16::try_from(chunk_count).map_err(|error| error.to_string())?;
    for (chunk_index, bytes) in frame_bytes.chunks(OBSERVER_FRAME_CHUNK_BYTES).enumerate() {
        let chunk = ComputerObservationFrameChunk {
            protocol_version: brood_computer_use::COMPUTER_USE_PROTOCOL_VERSION,
            task: frame.task.clone(),
            sequence: frame.sequence,
            chunk_index: u16::try_from(chunk_index).map_err(|error| error.to_string())?,
            chunk_count,
            bytes: bytes.to_vec(),
        };
        let encoded = serde_json::to_vec(&chunk).map_err(|error| error.to_string())?;
        if let Err(error) = state.send_computer_use_observer_message(&recipient, &encoded) {
            tracing::warn!(
                recipient,
                encoded_bytes = encoded.len(),
                %error,
                "computer-use observer frame chunk could not be queued"
            );
            state.computer_use_automation.observer_recipient = None;
            return Ok(());
        }
    }
    Ok(())
}

pub fn activate_window(state: &mut Brood, session: u64, pane: u64) -> Result<(), String> {
    let exists = state
        .presentation_window_summaries()
        .iter()
        .any(|window| window.session == session && window.pane == pane);
    if !exists {
        return Err("computer-use target window is outside the live session".to_string());
    }
    state.computer_use_automation.select_window(session, pane);
    configure_automation_target(state, pane);
    match capture_viewport(state, session, Some(pane)) {
        Ok(snapshot) => state.computer_use_automation.store_capture(snapshot),
        Err(error) => state.computer_use_automation.record_failure(&error),
    }
    if state.dmabuf.fast_path.is_none() {
        schedule_automation_frame_callback(state, Some(pane));
    }
    Ok(())
}

pub fn capture_current(state: &mut Brood) -> Result<(), String> {
    state.refresh_window_state();
    let session = state.active_surface_session().raw();
    let selected = state.computer_use_automation.selected_pane(state, session);
    let snapshot = capture_viewport(state, session, selected).map_err(|error| error.to_string())?;
    state.computer_use_automation.store_capture(snapshot);
    Ok(())
}

pub fn begin_adapter_action_batch(
    state: &mut Brood,
    batch: ComputerActionBatch,
    adapter_call_id: Option<String>,
    response: BatchResponse,
) {
    let prepared = (|| -> Result<(u64, u64), String> {
        if state.computer_use_automation.pending_batch.is_some() {
            return Err("another computer-use batch is executing".to_string());
        }
        let now_ms = monotonic_ms(state);
        if let Err(error) = state.computer_use.validate_batch(&batch, now_ms) {
            if error == brood_computer_use::ComputerUseError::ApprovalRequired
                && let Some(kind) = state.computer_use.required_approval_kind(&batch, now_ms)
            {
                let id = state.computer_use_automation.next_approval_id();
                let request = brood_computer_use::ComputerApprovalRequest {
                    id: id.clone(),
                    task: batch.task.clone(),
                    sequence: batch.expected_sequence,
                    kind,
                    prompt: format!("allow {kind:?} input for task {}", batch.task.0),
                    created_at_ms: now_ms,
                    expires_at_ms: now_ms.saturating_add(30_000),
                };
                state
                    .computer_use
                    .request_approval(request)
                    .map_err(|request_error| request_error.to_string())?;
                drop(publish_observer_update(state));
                return Err(format!("approval is required: {id}"));
            }
            return Err(error.to_string());
        }
        let target_pane = batch
            .target_window
            .or_else(|| {
                state
                    .computer_use_automation
                    .selected_pane(state, batch.target_session)
            })
            .ok_or_else(|| "computer-use target window is unavailable".to_string())?;
        let target = state
            .presentation_window_summaries()
            .into_iter()
            .find(|window| window.pane == target_pane && window.session == batch.target_session)
            .ok_or_else(|| "computer-use target window is outside the live session".to_string())?;
        let window = state
            .presentation_window_for_pane(target.pane)
            .ok_or_else(|| "computer-use target window disappeared".to_string())?;
        let size = window.geometry().size;
        validate_live_coordinates(&batch, size.w, size.h)?;
        let sequence = state
            .computer_use
            .commit_batch(&batch, now_ms)
            .map_err(|error| error.to_string())?;
        Ok((target_pane, sequence))
    })();
    let (target_pane, sequence) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            drop(response.send(Err(error)));
            return;
        }
    };
    state
        .computer_use_automation
        .select_window(batch.target_session, target_pane);
    state.computer_use_automation.next_execution_id = state
        .computer_use_automation
        .next_execution_id
        .saturating_add(1)
        .max(1);
    let execution_id = state.computer_use_automation.next_execution_id;
    let audit_task = batch.task.0.clone();
    let action_count = batch.actions.len();
    state.computer_use_automation.pending_batch = Some(PendingBatch {
        execution_id,
        batch,
        sequence,
        target_pane,
        next_action: 0,
        settle_started: None,
        results: Vec::new(),
        active_action: None,
        response,
        adapter_call_id,
        script: None,
    });
    tracing::info!(
        task = %audit_task,
        sequence,
        target_pane,
        execution_id,
        actions = action_count,
        "computer-use batch accepted"
    );
    schedule_next_action(state, Duration::ZERO, execution_id);
}

pub fn begin_computer_use_script(
    state: &mut Brood,
    request: ComputerScriptRequest,
    response: BatchResponse,
) {
    let now_ms = monotonic_ms(state);
    if let Err(error) = state.computer_use.validate_script(&request, now_ms) {
        if error == brood_computer_use::ComputerUseError::ApprovalRequired {
            let id = state.computer_use_automation.next_approval_id();
            let approval = brood_computer_use::ComputerApprovalRequest {
                id: id.clone(),
                task: request.task.clone(),
                sequence: request.expected_sequence,
                kind: brood_computer_use::ApprovalKind::Script,
                prompt: format!("allow script execution for task {}", request.task.0),
                created_at_ms: now_ms,
                expires_at_ms: now_ms.saturating_add(30_000),
            };
            let result = state
                .computer_use
                .request_approval(approval)
                .map_err(|approval_error| approval_error.to_string());
            drop(publish_observer_update(state));
            drop(response.send(match result {
                Ok(()) => Err(format!("approval is required: {id}")),
                Err(error) => Err(error),
            }));
            return;
        }
        drop(response.send(Err(error.to_string())));
        return;
    }
    if state.computer_use_automation.pending_batch.is_some() {
        drop(response.send(Err("another computer-use batch is executing".to_string())));
        return;
    }

    if let Some(checkpoint) = state
        .computer_use_automation
        .script_checkpoints
        .get(&request.task)
    {
        let matches = checkpoint.source == request.source
            && checkpoint.batch.protocol_version == request.protocol_version
            && checkpoint.batch.lease_generation == request.lease_generation
            && checkpoint.batch.expected_sequence == request.expected_sequence;
        if !matches {
            drop(response.send(Err(
                "script retry does not match the pending askUser checkpoint".to_string(),
            )));
            return;
        }
        if request.approval_proof.as_deref() != Some(checkpoint.approval_id.as_str())
            || !state.computer_use.approval_proof_valid(
                request.approval_proof.as_deref(),
                &request.task,
                brood_computer_use::ApprovalKind::Script,
                now_ms,
            )
        {
            drop(response.send(Err(format!(
                "askUser approval is required: {}",
                checkpoint.approval_id
            ))));
            return;
        }
        let Some(mut checkpoint) = state
            .computer_use_automation
            .script_checkpoints
            .remove(&request.task)
        else {
            drop(response.send(Err("askUser checkpoint disappeared".to_string())));
            return;
        };
        checkpoint.batch.approval_proof = request.approval_proof;
        begin_script_action_batch(state, checkpoint.batch, checkpoint.script, response);
        return;
    }

    let (limits, lease, windows, active_window) = match state.computer_use.task(&request.task) {
        Some(task) => {
            let Some(lease) = state
                .computer_use
                .lease()
                .filter(|lease| lease.task == request.task)
                .cloned()
            else {
                drop(response.send(Err("computer-use lease disappeared".to_string())));
                return;
            };
            let mut windows = state
                .computer_use_automation
                .latest()
                .map(|snapshot| {
                    snapshot
                        .windows
                        .iter()
                        .filter(|window| {
                            window.session == lease.session && lease.windows.contains(&window.pane)
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            windows.sort_by_key(|window| window.pane);
            let active_window = windows.iter().find(|window| window.active).cloned();
            (task.limits, lease, windows, active_window)
        }
        None => {
            drop(response.send(Err("computer-use task disappeared".to_string())));
            return;
        }
    };

    let execution = (|| -> Result<_, String> {
        if !state
            .computer_use_automation
            .script_isolates
            .contains_key(&request.task)
        {
            let path = computer_use_js_runtime_path();
            let module = fs::read(&path).map_err(|error| {
                format!("read computer-use JS runtime {}: {error}", path.display())
            })?;
            let isolate = ComputerUseJsIsolate::load(&module, script_runtime_config(limits))
                .map_err(|error| error.to_string())?;
            state
                .computer_use_automation
                .script_isolates
                .insert(request.task.clone(), isolate);
        }
        let isolate = state
            .computer_use_automation
            .script_isolates
            .get_mut(&request.task)
            .ok_or_else(|| "computer-use JS isolate disappeared".to_string())?;
        isolate
            .set_computer_context(
                &serde_json::to_value(&windows).map_err(|error| error.to_string())?,
                &serde_json::to_value(&active_window).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
        isolate
            .execute(&request.source)
            .map_err(|error| error.to_string())
    })();
    let execution = match execution {
        Ok(execution) => execution,
        Err(error) => {
            state
                .computer_use_automation
                .script_isolates
                .remove(&request.task);
            drop(response.send(Err(error)));
            return;
        }
    };
    let (actions, display_latest_observation, ask_user_prompt) =
        match script_actions(&execution.plan.calls, limits) {
            Ok(actions) => actions,
            Err(error) => {
                drop(response.send(Err(error)));
                return;
            }
        };
    let pending_script = PendingScript {
        console: execution.plan.console,
        value_json: execution.plan.value_json,
        exception: execution.plan.exception,
        metrics: execution.metrics,
        display_latest_observation,
    };

    let batch = ComputerActionBatch {
        protocol_version: request.protocol_version,
        task: request.task.clone(),
        lease_generation: request.lease_generation,
        expected_sequence: request.expected_sequence,
        target_session: lease.session,
        target_window: lease.windows.iter().next().copied(),
        actions: if actions.is_empty() {
            vec![ComputerAction::Screenshot]
        } else {
            actions
        },
        approval_proof: request.approval_proof.clone(),
    };
    if let Some(prompt) = ask_user_prompt {
        let id = state.computer_use_automation.next_approval_id();
        let approval = brood_computer_use::ComputerApprovalRequest {
            id: id.clone(),
            task: batch.task.clone(),
            sequence: batch.expected_sequence,
            kind: brood_computer_use::ApprovalKind::Script,
            prompt,
            created_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(30_000),
        };
        if let Err(error) = state.computer_use.request_approval(approval) {
            drop(response.send(Err(error.to_string())));
            return;
        }
        state.computer_use_automation.script_checkpoints.insert(
            batch.task.clone(),
            PendingScriptCheckpoint {
                approval_id: id.clone(),
                source: request.source,
                batch,
                script: pending_script,
            },
        );
        drop(publish_observer_update(state));
        drop(response.send(Err(format!("approval is required: {id}"))));
        return;
    }
    begin_script_action_batch(state, batch, pending_script, response);
}

fn script_runtime_config(limits: ComputerUseLimits) -> ComputerUseJsConfig {
    let max_output_bytes = usize::try_from(limits.max_output_bytes).unwrap_or(usize::MAX);
    let max_memory_bytes = usize::try_from(limits.max_script_memory_bytes).unwrap_or(usize::MAX);
    ComputerUseJsConfig {
        max_source_bytes: max_output_bytes,
        max_response_bytes: max_output_bytes,
        max_memory_bytes,
        max_host_calls: usize::from(limits.max_queued_host_calls),
        max_console_lines: max_output_bytes,
        fuel_per_call: u64::from(limits.max_script_cpu_ms).saturating_mul(10_000),
        wall_time: Duration::from_millis(u64::from(limits.max_script_wall_ms)),
        ..ComputerUseJsConfig::default()
    }
}

fn computer_use_js_runtime_path() -> PathBuf {
    std::env::var_os("BROOD_COMPUTER_USE_JS_RUNTIME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(
                "runtimes/computer-use-js/target/wasm32-wasip1/release/brood_computer_use_js_runtime.wasm",
            )
        })
}

fn script_actions(
    calls: &[serde_json::Value],
    limits: ComputerUseLimits,
) -> Result<(Vec<ComputerAction>, bool, Option<String>), String> {
    let mut actions = Vec::new();
    let mut display_latest = false;
    let mut display_images = 0_u16;
    let mut ask_user_prompt = None;
    for call in calls {
        let kind = call
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "script host call has no kind".to_string())?;
        match kind {
            "perform" => {
                let planned = call
                    .get("actions")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| "computer.perform requires an action array".to_string())?;
                for planned_action in planned {
                    let mut encoded = planned_action.clone();
                    let object = encoded
                        .as_object_mut()
                        .ok_or_else(|| "computer.perform action must be an object".to_string())?;
                    let kind = object
                        .remove("kind")
                        .ok_or_else(|| "computer.perform action has no kind".to_string())?;
                    object.insert("type".to_string(), kind);
                    actions.push(
                        serde_json::from_value(encoded)
                            .map_err(|error| format!("invalid script action: {error}"))?,
                    );
                }
            }
            "screenshot" => actions.push(ComputerAction::Screenshot),
            "display" => {
                display_images = display_images.saturating_add(1);
                if display_images > limits.max_display_images {
                    return Err(format!(
                        "script displayed more than {} images",
                        limits.max_display_images
                    ));
                }
                display_latest = true;
            }
            "ask_user" => {
                if ask_user_prompt.is_some() {
                    return Err("a script may queue only one askUser checkpoint".to_string());
                }
                let prompt = call
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .filter(|prompt| !prompt.trim().is_empty())
                    .ok_or_else(|| "askUser requires a non-empty prompt".to_string())?;
                ask_user_prompt = Some(prompt.to_string());
            }
            other => return Err(format!("unknown script host call {other}")),
        }
    }
    Ok((actions, display_latest, ask_user_prompt))
}

fn begin_script_action_batch(
    state: &mut Brood,
    batch: ComputerActionBatch,
    script: PendingScript,
    response: BatchResponse,
) {
    let prepared = (|| -> Result<(u64, u64), String> {
        let now_ms = monotonic_ms(state);
        state
            .computer_use
            .validate_batch(&batch, now_ms)
            .map_err(|error| error.to_string())?;
        let target_pane = batch
            .target_window
            .or_else(|| {
                state
                    .computer_use_automation
                    .selected_pane(state, batch.target_session)
            })
            .ok_or_else(|| "computer-use target window is unavailable".to_string())?;
        let window = state
            .presentation_window_for_pane(target_pane)
            .ok_or_else(|| "computer-use target window disappeared".to_string())?;
        validate_live_coordinates(&batch, window.geometry().size.w, window.geometry().size.h)?;
        let sequence = state
            .computer_use
            .commit_batch(&batch, now_ms)
            .map_err(|error| error.to_string())?;
        Ok((target_pane, sequence))
    })();
    let (target_pane, sequence) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            drop(response.send(Err(error)));
            return;
        }
    };
    state.computer_use_automation.next_execution_id = state
        .computer_use_automation
        .next_execution_id
        .saturating_add(1)
        .max(1);
    let execution_id = state.computer_use_automation.next_execution_id;
    state.computer_use_automation.pending_batch = Some(PendingBatch {
        execution_id,
        batch,
        sequence,
        target_pane,
        next_action: 0,
        settle_started: None,
        results: Vec::new(),
        active_action: None,
        response,
        adapter_call_id: None,
        script: Some(script),
    });
    schedule_next_action(state, Duration::ZERO, execution_id);
}

fn validate_live_coordinates(
    batch: &ComputerActionBatch,
    width: i32,
    height: i32,
) -> Result<(), String> {
    let contains = |x: f64, y: f64| {
        x >= 0.0 && y >= 0.0 && x < f64::from(width.max(0)) && y < f64::from(height.max(0))
    };
    let valid = batch.actions.iter().all(|action| match action {
        ComputerAction::Click { x, y, .. }
        | ComputerAction::DoubleClick { x, y, .. }
        | ComputerAction::Move { x, y }
        | ComputerAction::Scroll { x, y, .. } => contains(*x, *y),
        ComputerAction::Drag { path, .. } => path.iter().all(|point| contains(point.x, point.y)),
        ComputerAction::Type { text } => {
            text.chars().all(|character| ascii_key(character).is_some())
        }
        ComputerAction::Keypress { keys } => keys.iter().all(|key| named_key(key).is_some()),
        ComputerAction::Wait { .. } | ComputerAction::Screenshot => true,
    });
    valid.then_some(()).ok_or_else(|| {
        format!("computer-use coordinates exceed live content bounds {width}x{height}")
    })
}

fn run_next_action(state: &mut Brood, execution_id: u64) {
    let Some(mut pending) = state.computer_use_automation.pending_batch.take() else {
        return;
    };
    if pending.execution_id != execution_id {
        state.computer_use_automation.pending_batch = Some(pending);
        return;
    }
    let now_ms = monotonic_ms(state);
    if let Err(error) = state.computer_use.validate_live_lease(
        &pending.batch.task,
        pending.batch.lease_generation,
        now_ms,
    ) {
        finish_failed_batch(
            state,
            pending,
            ComputerUseFailure::InputRejected,
            &error.to_string(),
        );
        return;
    }
    if state
        .presentation_window_for_pane(pending.target_pane)
        .is_none()
    {
        finish_failed_batch(
            state,
            pending,
            ComputerUseFailure::TargetGone,
            "target window disappeared",
        );
        return;
    }
    if let Some(active) = pending.active_action.take() {
        continue_active_action(state, pending, active);
        return;
    }
    let Some(action) = pending.batch.actions.get(pending.next_action).cloned() else {
        pending.settle_started = Some(Instant::now());
        state.computer_use_automation.pending_batch = Some(pending);
        schedule_settle(state, execution_id);
        return;
    };
    let started_at_ms = now_ms;
    let delay = match &action {
        ComputerAction::Drag { path, button } => {
            let Some(first) = path.first() else {
                finish_failed_batch(
                    state,
                    pending,
                    ComputerUseFailure::InvalidAction,
                    "validated drag path was empty",
                );
                return;
            };
            pointer_move(
                state,
                pending.target_pane,
                first.x,
                first.y,
                monotonic_time_u32(state),
            );
            press_button(
                state,
                pending.target_pane,
                button_code(*button),
                monotonic_time_u32(state),
            );
            pending.active_action = Some(ActiveAction::Drag {
                path: path.clone(),
                button: *button,
                next_point: 1,
                started_at_ms,
            });
            Duration::from_millis(16)
        }
        ComputerAction::Keypress { keys } => {
            let resolved = keys
                .iter()
                .filter_map(|key| named_key(key))
                .map(smithay_keycode)
                .collect::<Vec<_>>();
            let time = monotonic_time_u32(state);
            for key in &resolved {
                press_key(state, *key, time);
            }
            pending.active_action = Some(ActiveAction::Keypress {
                keys: resolved,
                started_at_ms,
            });
            KEYPRESS_HOLD
        }
        _ => {
            execute_atomic_action(state, pending.target_pane, &action);
            complete_action(state, &mut pending, started_at_ms);
            match action {
                ComputerAction::Wait { duration_ms } => {
                    Duration::from_millis(u64::from(duration_ms))
                }
                ComputerAction::DoubleClick { .. } => Duration::from_millis(40),
                _ => Duration::ZERO,
            }
        }
    };
    state.computer_use_automation.pending_batch = Some(pending);
    schedule_next_action(state, delay, execution_id);
}

fn continue_active_action(state: &mut Brood, mut pending: PendingBatch, active: ActiveAction) {
    let execution_id = pending.execution_id;
    let started_at_ms = match active {
        ActiveAction::Drag {
            path,
            button,
            next_point,
            started_at_ms,
        } => {
            if let Some(point) = path.get(next_point) {
                pointer_move(
                    state,
                    pending.target_pane,
                    point.x,
                    point.y,
                    monotonic_time_u32(state),
                );
                pending.active_action = Some(ActiveAction::Drag {
                    path,
                    button,
                    next_point: next_point.saturating_add(1),
                    started_at_ms,
                });
                state.computer_use_automation.pending_batch = Some(pending);
                schedule_next_action(state, Duration::from_millis(16), execution_id);
                return;
            }
            release_button(
                state,
                pending.target_pane,
                button_code(button),
                monotonic_time_u32(state),
            );
            started_at_ms
        }
        ActiveAction::Keypress {
            keys,
            started_at_ms,
        } => {
            let time = monotonic_time_u32(state);
            for key in keys.into_iter().rev() {
                release_key(state, key, time);
            }
            started_at_ms
        }
    };
    complete_action(state, &mut pending, started_at_ms);
    state.computer_use_automation.pending_batch = Some(pending);
    schedule_next_action(state, Duration::ZERO, execution_id);
}

fn complete_action(state: &Brood, pending: &mut PendingBatch, started_at_ms: u64) {
    let action_index = u16::try_from(pending.next_action).unwrap_or(u16::MAX);
    pending.results.push(ComputerActionResult {
        task: pending.batch.task.clone(),
        sequence: pending.sequence,
        action_index,
        status: ActionStatus::Completed,
        started_at_ms,
        finished_at_ms: monotonic_ms(state),
        failure: None,
    });
    tracing::info!(
        task = %pending.batch.task.0,
        sequence = pending.sequence,
        action_index,
        "computer-use action completed"
    );
    pending.next_action = pending.next_action.saturating_add(1);
}

fn schedule_next_action(state: &mut Brood, delay: Duration, execution_id: u64) {
    drop(state.loop_handle.insert_source(
        smithay::reexports::calloop::timer::Timer::from_duration(delay),
        move |_, (), state| {
            run_next_action(state, execution_id);
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        },
    ));
}

fn execute_atomic_action(state: &mut Brood, pane: u64, action: &ComputerAction) {
    let time = monotonic_time_u32(state);
    match action {
        ComputerAction::Click { x, y, button } => {
            pointer_move(state, pane, *x, *y, time);
            pointer_click(state, pane, *button, time);
        }
        ComputerAction::DoubleClick { x, y, button } => {
            pointer_move(state, pane, *x, *y, time);
            pointer_click(state, pane, *button, time);
            pointer_click(state, pane, *button, time.saturating_add(1));
        }
        ComputerAction::Move { x, y } => pointer_move(state, pane, *x, *y, time),
        ComputerAction::Drag { .. } | ComputerAction::Keypress { .. } => {}
        ComputerAction::Scroll {
            x,
            y,
            delta_x,
            delta_y,
        } => {
            pointer_move(state, pane, *x, *y, time);
            let frame = crate::input::managed_continuous_axis_frame(time, *delta_x, *delta_y);
            state.forward_pointer_axis_to_pane(pane, frame);
        }
        ComputerAction::Type { text } => type_text(state, text, time),
        ComputerAction::Wait { .. } | ComputerAction::Screenshot => {}
    }
}

fn pointer_move(state: &mut Brood, pane: u64, x: f64, y: f64, time: u32) {
    let cursor_x = x
        .round()
        .clamp(0.0, f64::from(AUTOMATION_VIEWPORT_WIDTH.saturating_sub(1)))
        .to_u32()
        .unwrap_or_default();
    let cursor_y = y
        .round()
        .clamp(0.0, f64::from(AUTOMATION_VIEWPORT_HEIGHT.saturating_sub(1)))
        .to_u32()
        .unwrap_or_default();
    state
        .computer_use_automation
        .cursor_positions
        .insert(pane, (cursor_x, cursor_y));
    state.forward_pointer_motion_to_pane(pane, Point::<f64, Logical>::from((x, y)), time);
    crate::frame_export::request_redraw(state);
}

fn pointer_click(state: &mut Brood, pane: u64, button: PointerButton, time: u32) {
    let code = button_code(button);
    press_button(state, pane, code, time);
    release_button(state, pane, code, time.saturating_add(1));
}

const fn button_code(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => 0x110,
        PointerButton::Right => 0x111,
        PointerButton::Middle => 0x112,
    }
}

fn press_button(state: &mut Brood, pane: u64, code: u32, time: u32) {
    if !state
        .computer_use_automation
        .held_buttons
        .contains(&(pane, code))
    {
        state
            .computer_use_automation
            .held_buttons
            .push((pane, code));
    }
    state.forward_pointer_button_to_pane(pane, code, ButtonState::Pressed, time);
}

fn release_button(state: &mut Brood, pane: u64, code: u32, time: u32) {
    state.forward_pointer_button_to_pane(pane, code, ButtonState::Released, time);
    state
        .computer_use_automation
        .held_buttons
        .retain(|held| *held != (pane, code));
}

fn press_key(state: &mut Brood, key: Keycode, time: u32) {
    if !state.computer_use_automation.held_keys.contains(&key) {
        state.computer_use_automation.held_keys.push(key);
    }
    state.forward_keyboard_key(key, KeyState::Pressed, time);
}

fn release_key(state: &mut Brood, key: Keycode, time: u32) {
    state.forward_keyboard_key(key, KeyState::Released, time);
    state
        .computer_use_automation
        .held_keys
        .retain(|held| *held != key);
}

fn type_text(state: &mut Brood, text: &str, time: u32) {
    for (index, character) in text.chars().enumerate() {
        let Some((code, shift)) = ascii_key(character) else {
            continue;
        };
        let event_time = time.saturating_add(u32::try_from(index).unwrap_or(u32::MAX));
        let shift_key = smithay_keycode(42);
        if shift {
            press_key(state, shift_key, event_time);
        }
        press_key(state, smithay_keycode(code), event_time);
        release_key(state, smithay_keycode(code), event_time.saturating_add(1));
        if shift {
            release_key(state, shift_key, event_time.saturating_add(1));
        }
    }
}

fn smithay_keycode(evdev_code: u32) -> Keycode {
    evdev_code.saturating_add(8).into()
}

fn named_key(key: &str) -> Option<u32> {
    match key.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(29),
        "shift" => Some(42),
        "alt" => Some(56),
        "meta" | "super" => Some(125),
        "enter" | "return" => Some(28),
        "tab" => Some(15),
        "escape" | "esc" => Some(1),
        "backspace" => Some(14),
        "delete" => Some(111),
        "left" => Some(105),
        "right" => Some(106),
        "up" => Some(103),
        "down" => Some(108),
        value => value
            .chars()
            .next()
            .filter(|_| value.chars().count() == 1)
            .and_then(|ch| ascii_key(ch).map(|(code, _)| code)),
    }
}

const fn ascii_key(character: char) -> Option<(u32, bool)> {
    let lower = character.to_ascii_lowercase();
    let letter = match lower {
        'a' => 30,
        'b' => 48,
        'c' => 46,
        'd' => 32,
        'e' => 18,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'i' => 23,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'm' => 50,
        'n' => 49,
        'o' => 24,
        'p' => 25,
        'q' => 16,
        'r' => 19,
        's' => 31,
        't' => 20,
        'u' => 22,
        'v' => 47,
        'w' => 17,
        'x' => 45,
        'y' => 21,
        'z' => 44,
        _ => 0,
    };
    if letter != 0 {
        return Some((letter, character.is_ascii_uppercase()));
    }
    match character {
        '1' => Some((2, false)),
        '2' => Some((3, false)),
        '3' => Some((4, false)),
        '4' => Some((5, false)),
        '5' => Some((6, false)),
        '6' => Some((7, false)),
        '7' => Some((8, false)),
        '8' => Some((9, false)),
        '9' => Some((10, false)),
        '0' => Some((11, false)),
        ' ' => Some((57, false)),
        '\n' => Some((28, false)),
        '\t' => Some((15, false)),
        '-' => Some((12, false)),
        '_' => Some((12, true)),
        '=' => Some((13, false)),
        '+' => Some((13, true)),
        '[' => Some((26, false)),
        '{' => Some((26, true)),
        ']' => Some((27, false)),
        '}' => Some((27, true)),
        ';' => Some((39, false)),
        ':' => Some((39, true)),
        '\'' => Some((40, false)),
        '"' => Some((40, true)),
        '`' => Some((41, false)),
        '~' => Some((41, true)),
        '\\' => Some((43, false)),
        '|' => Some((43, true)),
        ',' => Some((51, false)),
        '<' => Some((51, true)),
        '.' => Some((52, false)),
        '>' => Some((52, true)),
        '/' => Some((53, false)),
        '?' => Some((53, true)),
        '!' => Some((2, true)),
        '@' => Some((3, true)),
        '#' => Some((4, true)),
        '$' => Some((5, true)),
        '%' => Some((6, true)),
        '^' => Some((7, true)),
        '&' => Some((8, true)),
        '*' => Some((9, true)),
        '(' => Some((10, true)),
        ')' => Some((11, true)),
        _ => None,
    }
}

fn schedule_settle(state: &mut Brood, execution_id: u64) {
    let target = state
        .computer_use_automation
        .pending_batch
        .as_ref()
        .map(|pending| pending.target_pane);
    send_automation_frame_callback(state, target);
    schedule_settle_check(state, SETTLE_QUIET, execution_id);
}

fn schedule_settle_check(state: &mut Brood, delay: Duration, execution_id: u64) {
    drop(state.loop_handle.insert_source(
        smithay::reexports::calloop::timer::Timer::from_duration(delay),
        move |_, (), state| {
            check_settled_batch(state, execution_id);
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        },
    ));
}

fn check_settled_batch(state: &mut Brood, execution_id: u64) {
    let now = Instant::now();
    let Some(started) = state
        .computer_use_automation
        .pending_batch
        .as_ref()
        .and_then(|pending| pending.settle_started)
    else {
        return;
    };
    if state
        .computer_use_automation
        .pending_batch
        .as_ref()
        .is_none_or(|pending| pending.execution_id != execution_id)
    {
        return;
    }
    let quiet = state
        .computer_use_automation
        .last_managed_commit_at
        .map_or(SETTLE_QUIET, |commit| now.duration_since(commit));
    let elapsed = now.duration_since(started);
    if quiet < SETTLE_QUIET && elapsed < SETTLE_TIMEOUT {
        schedule_settle_check(state, SETTLE_QUIET.saturating_sub(quiet), execution_id);
        return;
    }
    let timed_out = quiet < SETTLE_QUIET;
    finish_settled_batch(state, quiet, timed_out);
}

fn finish_settled_batch(state: &mut Brood, quiet: Duration, timed_out: bool) {
    let Some(pending) = state.computer_use_automation.pending_batch.take() else {
        return;
    };
    let capture = capture_viewport(
        state,
        pending.batch.target_session,
        Some(pending.target_pane),
    );
    let snapshot = match capture {
        Ok(snapshot) => {
            state.computer_use_automation.store_capture(snapshot);
            state.computer_use_automation.observation(
                &pending.batch.task,
                state.stats.input_events,
                !timed_out,
                u32::try_from(quiet.as_millis()).unwrap_or(u32::MAX),
                timed_out,
            )
        }
        Err(error) => {
            finish_failed_batch(
                state,
                pending,
                ComputerUseFailure::CaptureFailed,
                &error.to_string(),
            );
            return;
        }
    };
    cleanup_held_input(state);
    let payload = if let Some(script) = pending.script {
        let displayed_pngs = if script.display_latest_observation {
            snapshot
                .as_ref()
                .map(|observation| vec![observation.png.clone()])
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        serde_json::to_value(ComputerScriptResult {
            task: pending.batch.task.clone(),
            sequence: pending.sequence,
            console: script.console,
            displayed_pngs,
            value_json: script.value_json,
            exception: script.exception,
            metrics: ComputerScriptMetrics {
                wall_ms: script.metrics.wall_ms,
                cpu_ms: u32::try_from(script.metrics.fuel_consumed / 10_000).unwrap_or(u32::MAX),
                peak_memory_bytes: script.metrics.memory_bytes,
                host_calls: script.metrics.host_calls,
            },
        })
        .unwrap_or_else(|error| serde_json::json!({ "error": error.to_string() }))
    } else {
        serde_json::json!({
            "call_id": pending.adapter_call_id,
            "sequence": pending.sequence,
            "results": pending.results,
            "observation": snapshot,
        })
    };
    tracing::info!(
        task = %pending.batch.task.0,
        sequence = pending.sequence,
        actions = pending.results.len(),
        settled = !timed_out,
        quiet_ms = quiet.as_millis(),
        "computer-use batch completed"
    );
    drop(publish_observer_update(state));
    let encoded = serde_json::to_string(&payload).map_err(|error| error.to_string());
    drop(pending.response.send(encoded));
}

fn finish_failed_batch(
    state: &mut Brood,
    mut pending: PendingBatch,
    failure: ComputerUseFailure,
    detail: &str,
) {
    let action_index = u16::try_from(pending.next_action).unwrap_or(u16::MAX);
    let failure_for_log = failure.clone();
    pending.results.push(ComputerActionResult {
        task: pending.batch.task.clone(),
        sequence: pending.sequence,
        action_index,
        status: ActionStatus::Failed,
        started_at_ms: monotonic_ms(state),
        finished_at_ms: monotonic_ms(state),
        failure: Some(failure),
    });
    cleanup_held_input(state);
    let payload = serde_json::json!({
        "call_id": pending.adapter_call_id,
        "sequence": pending.sequence,
        "results": pending.results,
        "error": detail,
    });
    tracing::warn!(
        task = %pending.batch.task.0,
        sequence = pending.sequence,
        action_index,
        failure = ?failure_for_log,
        detail,
        "computer-use batch failed"
    );
    drop(publish_observer_update(state));
    drop(pending.response.send(Ok(payload.to_string())));
}

pub fn cleanup_held_input(state: &mut Brood) {
    let time = monotonic_time_u32(state);
    for key in std::mem::take(&mut state.computer_use_automation.held_keys)
        .into_iter()
        .rev()
    {
        state.forward_keyboard_key(key, KeyState::Released, time);
    }
    for (pane, button) in std::mem::take(&mut state.computer_use_automation.held_buttons)
        .into_iter()
        .rev()
    {
        state.forward_pointer_button_to_pane(pane, button, ButtonState::Released, time);
    }
}

pub fn drop_script_isolate(state: &mut Brood, task: &TaskId) -> bool {
    let isolate = state
        .computer_use_automation
        .script_isolates
        .remove(task)
        .is_some();
    let checkpoint = state
        .computer_use_automation
        .script_checkpoints
        .remove(task)
        .is_some();
    isolate || checkpoint
}

pub fn cancel_pending_batch(state: &mut Brood, task: &TaskId, detail: &str) -> bool {
    let checkpoint_cancelled = state
        .computer_use_automation
        .script_checkpoints
        .remove(task)
        .is_some();
    if state
        .computer_use_automation
        .pending_batch
        .as_ref()
        .is_none_or(|pending| pending.batch.task != *task)
    {
        return checkpoint_cancelled;
    }
    let Some(mut pending) = state.computer_use_automation.pending_batch.take() else {
        return checkpoint_cancelled;
    };
    pending.results.push(ComputerActionResult {
        task: pending.batch.task.clone(),
        sequence: pending.sequence,
        action_index: u16::try_from(pending.next_action).unwrap_or(u16::MAX),
        status: ActionStatus::Cancelled,
        started_at_ms: monotonic_ms(state),
        finished_at_ms: monotonic_ms(state),
        failure: Some(ComputerUseFailure::Cancelled),
    });
    cleanup_held_input(state);
    let payload = serde_json::json!({
        "call_id": pending.adapter_call_id,
        "sequence": pending.sequence,
        "results": pending.results,
        "error": detail,
    });
    tracing::info!(
        task = %pending.batch.task.0,
        sequence = pending.sequence,
        detail,
        "computer-use batch cancelled"
    );
    drop(pending.response.send(Ok(payload.to_string())));
    true
}

fn monotonic_ms(state: &Brood) -> u64 {
    state
        .start_time
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn monotonic_time_u32(state: &Brood) -> u32 {
    u32::try_from(monotonic_ms(state)).unwrap_or(u32::MAX)
}

pub fn capture_after_commit(state: &mut Brood) {
    state.computer_use_automation.last_managed_commit_at = Some(Instant::now());
    if state.computer_use_automation.pending_batch.is_some() || state.computer_use.lease().is_some()
    {
        return;
    }
    let now = Instant::now();
    if !state.computer_use_automation.capture_due(now) {
        return;
    }
    let session = state.active_surface_session().raw();
    let selected = state.computer_use_automation.selected_pane(state, session);
    match capture_viewport(state, session, selected) {
        Ok(snapshot) => {
            state.computer_use_automation.store_capture(snapshot);
            if let Some(task) = state.computer_use.lease().map(|lease| lease.task.clone()) {
                let _ = state.computer_use_automation.observation(
                    &task,
                    state.stats.input_events,
                    false,
                    0,
                    false,
                );
            }
            if state.dmabuf.fast_path.is_none() && state.computer_use.lease().is_none() {
                schedule_automation_frame_callback(state, selected);
            }
        }
        Err(error) => state.computer_use_automation.record_failure(&error),
    }
}

fn configure_automation_target(state: &mut Brood, pane: u64) {
    state.configure_automation_pane_size(
        pane,
        i32::try_from(AUTOMATION_VIEWPORT_WIDTH).unwrap_or(i32::MAX),
        i32::try_from(AUTOMATION_VIEWPORT_HEIGHT).unwrap_or(i32::MAX),
    );
}

fn schedule_automation_frame_callback(state: &mut Brood, pane: Option<u64>) {
    if state.computer_use_automation.frame_callback_pending {
        return;
    }
    state.computer_use_automation.frame_callback_pending = true;
    let inserted = state.loop_handle.insert_source(
        smithay::reexports::calloop::timer::Timer::from_duration(CAPTURE_INTERVAL),
        move |_, (), state| {
            state.computer_use_automation.frame_callback_pending = false;
            send_automation_frame_callback(state, pane);
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        },
    );
    if inserted.is_err() {
        state.computer_use_automation.frame_callback_pending = false;
    }
}

fn send_automation_frame_callback(state: &mut Brood, pane: Option<u64>) {
    let Some(window) = pane.and_then(|pane| state.presentation_window_for_pane(pane)) else {
        return;
    };
    let size: Size<i32, Physical> = (
        i32::try_from(AUTOMATION_VIEWPORT_WIDTH).unwrap_or(i32::MAX),
        i32::try_from(AUTOMATION_VIEWPORT_HEIGHT).unwrap_or(i32::MAX),
    )
        .into();
    let output = automation_output(size);
    window.send_frame(
        &output,
        state.start_time.elapsed(),
        Some(Duration::ZERO),
        |_, _| Some(output.clone()),
    );
    if let Some(feedback) = state.dmabuf.default_feedback.as_ref() {
        window.send_dmabuf_feedback(&output, |_, _| Some(output.clone()), |_, _| feedback);
    }
    state.stats.record_client_frame_callbacks(1);
    drop(state.display_handle.flush_clients());
}

pub fn capture_viewport(
    state: &Brood,
    session: u64,
    selected_pane: Option<u64>,
) -> Result<AutomationSnapshot, FrameExportFailure> {
    let exporter = state.dmabuf.exporter.clone().ok_or_else(|| {
        FrameExportFailure::new("capture_automation_viewport", "exporter unavailable")
    })?;
    let active_window = selected_pane.and_then(|pane| state.presentation_window_for_pane(pane));
    let size: Size<i32, Physical> = (
        i32::try_from(AUTOMATION_VIEWPORT_WIDTH).unwrap_or(i32::MAX),
        i32::try_from(AUTOMATION_VIEWPORT_HEIGHT).unwrap_or(i32::MAX),
    )
        .into();
    let output = automation_output(size);
    let mut space = Space::default();
    space.map_output(&output, (0, 0));
    if let Some(window) = active_window.as_ref() {
        space.map_element(window.clone(), (0, 0), true);
    }

    let frame = {
        let mut exporter = exporter.borrow_mut();
        let mut renderbuffer = Offscreen::<GlesRenderbuffer>::create_buffer(
            exporter.renderer(),
            Fourcc::Abgr8888,
            (size.w, size.h).into(),
        )
        .map_err(|error| FrameExportFailure::new("allocate_automation_viewport", error))?;
        let renderer = exporter.renderer();
        let mut framebuffer = renderer
            .bind(&mut renderbuffer)
            .map_err(|error| FrameExportFailure::new("bind_automation_viewport", error))?;
        let mut damage =
            smithay::backend::renderer::damage::OutputDamageTracker::from_output(&output);
        let sync = smithay::desktop::space::render_output::<_, SolidColorRenderElement, _, _>(
            &output,
            renderer,
            &mut framebuffer,
            1.0,
            0,
            [&space],
            &[],
            &mut damage,
            [0.02, 0.02, 0.025, 1.0],
        )
        .map_err(|error| FrameExportFailure::new("render_automation_viewport", error))?
        .sync;
        sync.wait()
            .map_err(|error| FrameExportFailure::new("wait_automation_viewport", error))?;
        let frame = buffer_readback::read_gles_framebuffer(renderer, &framebuffer, size)
            .map_err(|error| FrameExportFailure::new("read_automation_viewport", error))?;
        drop(framebuffer);
        drop(sync);
        frame
    };
    let mut png = Vec::new();
    PngEncoder::new(Cursor::new(&mut png))
        .write_image(
            &frame.bytes,
            frame.width,
            frame.height,
            ColorType::Rgba8.into(),
        )
        .map_err(|error| FrameExportFailure::new("encode_automation_png", error))?;
    if png.len() > MAX_OBSERVATION_PNG_BYTES {
        return Err(FrameExportFailure::new(
            "encode_automation_png",
            format!(
                "PNG is {} bytes; limit is {MAX_OBSERVATION_PNG_BYTES}",
                png.len()
            ),
        ));
    }

    let mut windows = state
        .space
        .elements()
        .filter_map(|window| {
            let pane = state.window_pane_id(window)?;
            let window_session = state.window_session_id(window)?;
            (window_session == session).then(|| {
                let geometry = window.geometry();
                ComputerWindow {
                    pane,
                    session: window_session,
                    application_id: state.window_app_id(window).unwrap_or_default(),
                    title: state.window_title(window).unwrap_or_default(),
                    bounds: ComputerRect {
                        x: 0,
                        y: 0,
                        width: u32::try_from(geometry.size.w.max(0))
                            .unwrap_or(u32::MAX)
                            .min(AUTOMATION_VIEWPORT_WIDTH),
                        height: u32::try_from(geometry.size.h.max(0))
                            .unwrap_or(u32::MAX)
                            .min(AUTOMATION_VIEWPORT_HEIGHT),
                    },
                    content_width: u32::try_from(geometry.size.w.max(0)).unwrap_or(u32::MAX),
                    content_height: u32::try_from(geometry.size.h.max(0)).unwrap_or(u32::MAX),
                    active: Some(pane) == selected_pane,
                }
            })
        })
        .collect::<Vec<_>>();
    windows.sort_by_key(|window| window.pane);
    Ok(AutomationSnapshot {
        captured_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        frame_sequence: 0,
        active_window: selected_pane,
        windows,
        png,
    })
}

fn automation_output(size: Size<i32, Physical>) -> Output {
    let output = Output::new(
        "brood-automation".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Brood".into(),
            model: "Server automation viewport".into(),
        },
    );
    let mode = Mode {
        size,
        refresh: SESSION_OUTPUT_REFRESH_MILLIHZ,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(smithay::output::Scale::Fractional(1.0)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    output
}

#[cfg(test)]
mod tests {
    use brood_computer_use::COMPUTER_USE_PROTOCOL_VERSION;

    use super::*;

    fn snapshot() -> AutomationSnapshot {
        AutomationSnapshot {
            captured_at_ms: 1,
            frame_sequence: 0,
            active_window: Some(7),
            windows: Vec::new(),
            png: vec![137, 80, 78, 71],
        }
    }

    #[test]
    fn observation_queue_drops_oldest_and_sequences_monotonically() {
        let mut runtime = AutomationRuntime::default();
        let task = TaskId("queue-proof".to_string());
        for input_sequence in 1..=4 {
            runtime.store_capture(snapshot());
            let observation = runtime
                .observation(&task, input_sequence, true, 75, false)
                .expect("stored capture produces observation");
            assert_eq!(observation.sequence, input_sequence);
            assert_eq!(observation.settle.frame_sequence, input_sequence);
        }
        assert_eq!(runtime.queued_observations(), MAX_QUEUED_OBSERVATIONS);
        assert_eq!(runtime.observations_dropped_backpressure, 2);
        assert_eq!(
            runtime
                .observations
                .front()
                .map(|observation| observation.sequence),
            Some(3)
        );
    }

    #[test]
    fn live_validation_rejects_content_overflow_and_unsupported_text() {
        let base = ComputerActionBatch {
            protocol_version: COMPUTER_USE_PROTOCOL_VERSION,
            task: TaskId("validation-proof".to_string()),
            lease_generation: 1,
            expected_sequence: 1,
            target_session: 1,
            target_window: Some(7),
            actions: vec![ComputerAction::Click {
                x: 101.0,
                y: 10.0,
                button: PointerButton::Left,
            }],
            approval_proof: None,
        };
        assert!(validate_live_coordinates(&base, 100, 100).is_err());
        let mut unsupported = base;
        unsupported.actions = vec![ComputerAction::Type {
            text: "lambda: λ".to_string(),
        }];
        assert!(validate_live_coordinates(&unsupported, 100, 100).is_err());
    }
}
