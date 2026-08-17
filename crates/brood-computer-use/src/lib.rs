//! Server-owned domain model and lifecycle for Brood computer use.
//!
//! This crate intentionally has no compositor, transport, model-provider, or
//! presentation-client dependency. `broodd` owns one [`ComputerUseSupervisor`]
//! and adapters translate their wire shapes into these types.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const COMPUTER_USE_PROTOCOL_VERSION: u32 = 1;
pub const AUTOMATION_VIEWPORT_WIDTH: u32 = 1440;
pub const AUTOMATION_VIEWPORT_HEIGHT: u32 = 900;
pub const MAX_OBSERVATION_PNG_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_QUEUED_OBSERVATIONS: usize = 2;
pub const MIN_CAPTURE_INTERVAL_MS: u64 = 50;

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerUseScope {
    pub session: u64,
    #[serde(default)]
    pub windows: BTreeSet<u64>,
}

impl ComputerUseScope {
    #[must_use]
    pub fn permits_window(&self, pane: u64) -> bool {
        self.windows.is_empty() || self.windows.contains(&pane)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanInputPolicy {
    Pause,
    Revoke,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerUsePolicy {
    pub human_input: HumanInputPolicy,
    pub require_approval: BTreeSet<ApprovalKind>,
}

impl Default for ComputerUsePolicy {
    fn default() -> Self {
        Self {
            human_input: HumanInputPolicy::Pause,
            require_approval: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Pointer,
    Keyboard,
    Script,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerUseLimits {
    pub max_actions_per_batch: u16,
    pub max_script_wall_ms: u32,
    pub max_script_cpu_ms: u32,
    pub max_script_memory_bytes: u64,
    pub max_output_bytes: u32,
    pub max_display_images: u16,
    pub max_queued_host_calls: u16,
}

impl ComputerUseLimits {
    pub const DEFAULT: Self = Self {
        max_actions_per_batch: 64,
        max_script_wall_ms: 30_000,
        max_script_cpu_ms: 5_000,
        max_script_memory_bytes: 64 * 1024 * 1024,
        max_output_bytes: 1024 * 1024,
        max_display_images: 16,
        max_queued_host_calls: 64,
    };
    pub const HARD_CEILING: Self = Self {
        max_actions_per_batch: 256,
        max_script_wall_ms: 120_000,
        max_script_cpu_ms: 30_000,
        max_script_memory_bytes: 256 * 1024 * 1024,
        max_output_bytes: 8 * 1024 * 1024,
        max_display_images: 64,
        max_queued_host_calls: 256,
    };

    const fn validate(self) -> Result<(), ComputerUseError> {
        let ceiling = Self::HARD_CEILING;
        if self.max_actions_per_batch == 0
            || self.max_actions_per_batch > ceiling.max_actions_per_batch
            || self.max_script_wall_ms == 0
            || self.max_script_wall_ms > ceiling.max_script_wall_ms
            || self.max_script_cpu_ms == 0
            || self.max_script_cpu_ms > ceiling.max_script_cpu_ms
            || self.max_script_memory_bytes == 0
            || self.max_script_memory_bytes > ceiling.max_script_memory_bytes
            || self.max_output_bytes == 0
            || self.max_output_bytes > ceiling.max_output_bytes
            || self.max_display_images == 0
            || self.max_display_images > ceiling.max_display_images
            || self.max_queued_host_calls == 0
            || self.max_queued_host_calls > ceiling.max_queued_host_calls
        {
            return Err(ComputerUseError::LimitsExceeded);
        }
        Ok(())
    }
}

impl Default for ComputerUseLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Ready,
    Running,
    Paused,
    WaitingApproval,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerUseTask {
    pub protocol_version: u32,
    pub id: TaskId,
    pub principal: String,
    pub scope: ComputerUseScope,
    pub policy: ComputerUsePolicy,
    pub limits: ComputerUseLimits,
    pub state: TaskState,
    pub next_sequence: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerUseLease {
    pub task: TaskId,
    pub generation: u64,
    pub session: u64,
    pub windows: BTreeSet<u64>,
    pub acquired_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComputerAction {
    Click {
        x: f64,
        y: f64,
        button: PointerButton,
    },
    DoubleClick {
        x: f64,
        y: f64,
        button: PointerButton,
    },
    Scroll {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
    },
    Type {
        text: String,
    },
    Wait {
        duration_ms: u32,
    },
    Keypress {
        keys: Vec<String>,
    },
    Drag {
        path: Vec<ComputerPoint>,
        button: PointerButton,
    },
    Move {
        x: f64,
        y: f64,
    },
    Screenshot,
}

impl ComputerAction {
    const fn approval_kind(&self) -> Option<ApprovalKind> {
        match self {
            Self::Click { .. }
            | Self::DoubleClick { .. }
            | Self::Scroll { .. }
            | Self::Drag { .. }
            | Self::Move { .. } => Some(ApprovalKind::Pointer),
            Self::Type { .. } | Self::Keypress { .. } => Some(ApprovalKind::Keyboard),
            Self::Wait { .. } | Self::Screenshot => None,
        }
    }

    fn validate(&self) -> Result<(), ComputerUseError> {
        let point = |x: f64, y: f64| {
            x.is_finite()
                && y.is_finite()
                && x >= 0.0
                && y >= 0.0
                && x < f64::from(AUTOMATION_VIEWPORT_WIDTH)
                && y < f64::from(AUTOMATION_VIEWPORT_HEIGHT)
        };
        let valid = match self {
            Self::Click { x, y, .. } | Self::DoubleClick { x, y, .. } | Self::Move { x, y } => {
                point(*x, *y)
            }
            Self::Scroll {
                x,
                y,
                delta_x,
                delta_y,
            } => point(*x, *y) && delta_x.is_finite() && delta_y.is_finite(),
            Self::Type { text } => text.len() <= 64 * 1024,
            Self::Wait { duration_ms } => *duration_ms <= 30_000,
            Self::Keypress { keys } => !keys.is_empty() && keys.len() <= 32,
            Self::Drag { path, .. } => {
                path.len() >= 2 && path.len() <= 256 && path.iter().all(|p| point(p.x, p.y))
            }
            Self::Screenshot => true,
        };
        if valid {
            Ok(())
        } else {
            Err(ComputerUseError::InvalidAction)
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct ComputerPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ComputerActionBatch {
    pub protocol_version: u32,
    pub task: TaskId,
    pub lease_generation: u64,
    pub expected_sequence: u64,
    pub target_session: u64,
    pub target_window: Option<u64>,
    pub actions: Vec<ComputerAction>,
    pub approval_proof: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerActionResult {
    pub task: TaskId,
    pub sequence: u64,
    pub action_index: u16,
    pub status: ActionStatus,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub failure: Option<ComputerUseFailure>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerWindow {
    pub pane: u64,
    pub session: u64,
    pub application_id: String,
    pub title: String,
    pub bounds: ComputerRect,
    pub content_width: u32,
    pub content_height: u32,
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SettleEvidence {
    pub settled: bool,
    pub quiet_ms: u32,
    pub timed_out: bool,
    pub frame_sequence: u64,
    pub input_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerObservation {
    pub protocol_version: u32,
    pub task: TaskId,
    pub sequence: u64,
    pub captured_at_ms: u64,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub active_window: Option<u64>,
    pub windows: Vec<ComputerWindow>,
    pub png: Vec<u8>,
    pub settle: SettleEvidence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerScriptRequest {
    pub protocol_version: u32,
    pub task: TaskId,
    pub lease_generation: u64,
    pub expected_sequence: u64,
    pub source: String,
    pub approval_proof: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerScriptMetrics {
    pub wall_ms: u32,
    pub cpu_ms: u32,
    pub peak_memory_bytes: u64,
    pub host_calls: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerScriptResult {
    pub task: TaskId,
    pub sequence: u64,
    pub console: Vec<String>,
    pub displayed_pngs: Vec<Vec<u8>>,
    pub value_json: Option<String>,
    pub exception: Option<String>,
    pub metrics: ComputerScriptMetrics,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerApprovalRequest {
    pub id: String,
    pub task: TaskId,
    pub sequence: u64,
    pub kind: ApprovalKind,
    pub prompt: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerApprovalDecision {
    pub request_id: String,
    pub approved: bool,
    pub principal: String,
    pub decided_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComputerCancellation {
    pub task: TaskId,
    pub reason: String,
    pub requested_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, Error, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerUseError {
    #[error("unsupported computer-use protocol version")]
    UnsupportedVersion,
    #[error("task already exists")]
    TaskExists,
    #[error("task was not found")]
    TaskNotFound,
    #[error("task state does not allow this operation")]
    InvalidTransition,
    #[error("the daemon-wide lease is held by another task")]
    LeaseBusy,
    #[error("lease is missing, stale, or expired")]
    StaleLease,
    #[error("request is outside the task scope")]
    ScopeDenied,
    #[error("request sequence is stale")]
    StaleSequence,
    #[error("runtime limits exceed a hard ceiling")]
    LimitsExceeded,
    #[error("action batch is empty or too large")]
    InvalidBatch,
    #[error("action is invalid")]
    InvalidAction,
    #[error("approval is required")]
    ApprovalRequired,
    #[error("approval request was not found")]
    ApprovalNotFound,
    #[error("approval request expired")]
    ApprovalExpired,
    #[error("script request is invalid")]
    InvalidScript,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerUseFailure {
    InvalidAction,
    TargetGone,
    InputRejected,
    CaptureFailed,
    SettleTimeout,
    Cancelled,
    Internal,
}

#[derive(Debug, Default)]
pub struct ComputerUseSupervisor {
    tasks: BTreeMap<TaskId, ComputerUseTask>,
    lease: Option<ComputerUseLease>,
    approval_requests: BTreeMap<String, ComputerApprovalRequest>,
    granted_approvals: BTreeMap<String, (TaskId, ApprovalKind, u64)>,
    next_lease_generation: u64,
}

impl ComputerUseSupervisor {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_lease_generation: 1,
            ..Self::default()
        }
    }

    pub fn create_task(
        &mut self,
        id: &TaskId,
        principal: String,
        scope: ComputerUseScope,
        policy: ComputerUsePolicy,
        limits: ComputerUseLimits,
        now_ms: u64,
    ) -> Result<&ComputerUseTask, ComputerUseError> {
        limits.validate()?;
        if self.tasks.contains_key(id) {
            return Err(ComputerUseError::TaskExists);
        }
        let task = ComputerUseTask {
            protocol_version: COMPUTER_USE_PROTOCOL_VERSION,
            id: id.clone(),
            principal,
            scope,
            policy,
            limits,
            state: TaskState::Ready,
            next_sequence: 1,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        self.tasks.insert(id.clone(), task);
        self.tasks.get(id).ok_or(ComputerUseError::TaskNotFound)
    }

    #[must_use]
    pub fn task(&self, id: &TaskId) -> Option<&ComputerUseTask> {
        self.tasks.get(id)
    }

    #[must_use]
    pub const fn lease(&self) -> Option<&ComputerUseLease> {
        self.lease.as_ref()
    }

    #[must_use]
    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn acquire_lease(
        &mut self,
        id: &TaskId,
        now_ms: u64,
        duration_ms: u64,
    ) -> Result<ComputerUseLease, ComputerUseError> {
        self.expire_lease(now_ms);
        if self.lease.is_some() {
            return Err(ComputerUseError::LeaseBusy);
        }
        let task = self
            .tasks
            .get_mut(id)
            .ok_or(ComputerUseError::TaskNotFound)?;
        if !matches!(task.state, TaskState::Ready | TaskState::Paused) || duration_ms == 0 {
            return Err(ComputerUseError::InvalidTransition);
        }
        let lease = ComputerUseLease {
            task: id.clone(),
            generation: self.next_lease_generation,
            session: task.scope.session,
            windows: task.scope.windows.clone(),
            acquired_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(duration_ms),
        };
        self.next_lease_generation = self.next_lease_generation.saturating_add(1);
        task.state = TaskState::Running;
        task.updated_at_ms = now_ms;
        self.lease = Some(lease.clone());
        Ok(lease)
    }

    pub fn renew_lease(
        &mut self,
        id: &TaskId,
        generation: u64,
        now_ms: u64,
        duration_ms: u64,
    ) -> Result<ComputerUseLease, ComputerUseError> {
        self.require_lease(id, generation, now_ms)?;
        if duration_ms == 0 {
            return Err(ComputerUseError::StaleLease);
        }
        let lease = self.lease.as_mut().ok_or(ComputerUseError::StaleLease)?;
        lease.expires_at_ms = now_ms.saturating_add(duration_ms);
        Ok(lease.clone())
    }

    pub fn pause(&mut self, id: &TaskId, now_ms: u64) -> Result<(), ComputerUseError> {
        self.transition(id, TaskState::Paused, now_ms)?;
        self.revoke_lease_for(id);
        Ok(())
    }

    pub fn resume(&mut self, id: &TaskId, now_ms: u64) -> Result<(), ComputerUseError> {
        let task = self
            .tasks
            .get_mut(id)
            .ok_or(ComputerUseError::TaskNotFound)?;
        if task.state != TaskState::Paused {
            return Err(ComputerUseError::InvalidTransition);
        }
        task.state = TaskState::Ready;
        task.updated_at_ms = now_ms;
        Ok(())
    }

    pub fn stop(&mut self, id: &TaskId, now_ms: u64) -> Result<(), ComputerUseError> {
        let task = self
            .tasks
            .get_mut(id)
            .ok_or(ComputerUseError::TaskNotFound)?;
        if matches!(task.state, TaskState::Stopped | TaskState::Failed) {
            return Err(ComputerUseError::InvalidTransition);
        }
        task.state = TaskState::Stopped;
        task.updated_at_ms = now_ms;
        self.revoke_lease_for(id);
        Ok(())
    }

    pub fn fail(&mut self, id: &TaskId, now_ms: u64) -> Result<(), ComputerUseError> {
        let task = self
            .tasks
            .get_mut(id)
            .ok_or(ComputerUseError::TaskNotFound)?;
        if matches!(task.state, TaskState::Stopped | TaskState::Failed) {
            return Err(ComputerUseError::InvalidTransition);
        }
        task.state = TaskState::Failed;
        task.updated_at_ms = now_ms;
        self.revoke_lease_for(id);
        Ok(())
    }

    pub fn request_approval(
        &mut self,
        request: ComputerApprovalRequest,
    ) -> Result<(), ComputerUseError> {
        let task = self
            .tasks
            .get_mut(&request.task)
            .ok_or(ComputerUseError::TaskNotFound)?;
        if task.state != TaskState::Running || request.sequence != task.next_sequence {
            return Err(ComputerUseError::InvalidTransition);
        }
        task.state = TaskState::WaitingApproval;
        task.updated_at_ms = request.created_at_ms;
        self.approval_requests.insert(request.id.clone(), request);
        Ok(())
    }

    pub fn decide_approval(
        &mut self,
        decision: &ComputerApprovalDecision,
    ) -> Result<(), ComputerUseError> {
        let request = self
            .approval_requests
            .remove(&decision.request_id)
            .ok_or(ComputerUseError::ApprovalNotFound)?;
        if request.expires_at_ms <= decision.decided_at_ms {
            return Err(ComputerUseError::ApprovalExpired);
        }
        let task = self
            .tasks
            .get_mut(&request.task)
            .ok_or(ComputerUseError::TaskNotFound)?;
        if task.state != TaskState::WaitingApproval {
            return Err(ComputerUseError::InvalidTransition);
        }
        if decision.approved {
            self.granted_approvals.insert(
                request.id,
                (request.task, request.kind, request.expires_at_ms),
            );
            task.state = TaskState::Running;
        } else {
            task.state = TaskState::Paused;
            self.lease = None;
        }
        task.updated_at_ms = decision.decided_at_ms;
        Ok(())
    }

    pub fn validate_batch(
        &self,
        batch: &ComputerActionBatch,
        now_ms: u64,
    ) -> Result<(), ComputerUseError> {
        if batch.protocol_version != COMPUTER_USE_PROTOCOL_VERSION {
            return Err(ComputerUseError::UnsupportedVersion);
        }
        let task = self
            .tasks
            .get(&batch.task)
            .ok_or(ComputerUseError::TaskNotFound)?;
        if task.state != TaskState::Running {
            return Err(ComputerUseError::InvalidTransition);
        }
        self.require_lease(&batch.task, batch.lease_generation, now_ms)?;
        if batch.expected_sequence != task.next_sequence {
            return Err(ComputerUseError::StaleSequence);
        }
        if batch.target_session != task.scope.session
            || batch
                .target_window
                .is_some_and(|pane| !task.scope.permits_window(pane))
        {
            return Err(ComputerUseError::ScopeDenied);
        }
        if batch.actions.is_empty()
            || batch.actions.len() > usize::from(task.limits.max_actions_per_batch)
        {
            return Err(ComputerUseError::InvalidBatch);
        }
        for action in &batch.actions {
            action.validate()?;
            let kind = action.approval_kind();
            if kind.is_some_and(|kind| task.policy.require_approval.contains(&kind))
                && !self.approval_valid(batch.approval_proof.as_deref(), &batch.task, kind, now_ms)
            {
                return Err(ComputerUseError::ApprovalRequired);
            }
        }
        Ok(())
    }

    pub fn validate_live_lease(
        &mut self,
        task: &TaskId,
        generation: u64,
        now_ms: u64,
    ) -> Result<(), ComputerUseError> {
        self.expire_lease(now_ms);
        let state = self
            .tasks
            .get(task)
            .ok_or(ComputerUseError::TaskNotFound)?
            .state;
        if state != TaskState::Running {
            return Err(ComputerUseError::InvalidTransition);
        }
        self.require_lease(task, generation, now_ms)
    }

    pub fn commit_batch(
        &mut self,
        batch: &ComputerActionBatch,
        now_ms: u64,
    ) -> Result<u64, ComputerUseError> {
        self.validate_batch(batch, now_ms)?;
        let task = self
            .tasks
            .get_mut(&batch.task)
            .ok_or(ComputerUseError::TaskNotFound)?;
        let sequence = task.next_sequence;
        task.next_sequence = task.next_sequence.saturating_add(1);
        task.updated_at_ms = now_ms;
        if let Some(proof) = &batch.approval_proof {
            self.granted_approvals.remove(proof);
        }
        Ok(sequence)
    }

    pub fn validate_script(
        &self,
        request: &ComputerScriptRequest,
        now_ms: u64,
    ) -> Result<(), ComputerUseError> {
        if request.protocol_version != COMPUTER_USE_PROTOCOL_VERSION {
            return Err(ComputerUseError::UnsupportedVersion);
        }
        let task = self
            .tasks
            .get(&request.task)
            .ok_or(ComputerUseError::TaskNotFound)?;
        if task.state != TaskState::Running {
            return Err(ComputerUseError::InvalidTransition);
        }
        self.require_lease(&request.task, request.lease_generation, now_ms)?;
        if request.expected_sequence != task.next_sequence {
            return Err(ComputerUseError::StaleSequence);
        }
        let max_source_bytes = usize::try_from(task.limits.max_output_bytes).unwrap_or(0);
        if request.source.is_empty() || request.source.len() > max_source_bytes {
            return Err(ComputerUseError::InvalidScript);
        }
        if task.policy.require_approval.contains(&ApprovalKind::Script)
            && !self.approval_valid(
                request.approval_proof.as_deref(),
                &request.task,
                Some(ApprovalKind::Script),
                now_ms,
            )
        {
            return Err(ComputerUseError::ApprovalRequired);
        }
        Ok(())
    }

    pub fn handle_human_input(&mut self, now_ms: u64) -> Option<TaskId> {
        let task_id = self.lease.as_ref()?.task.clone();
        let policy = self.tasks.get(&task_id)?.policy.human_input.clone();
        if let Some(task) = self.tasks.get_mut(&task_id) {
            task.state = match policy {
                HumanInputPolicy::Pause => TaskState::Paused,
                HumanInputPolicy::Revoke => TaskState::Ready,
            };
            task.updated_at_ms = now_ms;
        }
        self.lease = None;
        Some(task_id)
    }

    fn transition(
        &mut self,
        id: &TaskId,
        to: TaskState,
        now_ms: u64,
    ) -> Result<(), ComputerUseError> {
        let task = self
            .tasks
            .get_mut(id)
            .ok_or(ComputerUseError::TaskNotFound)?;
        let allowed = matches!(
            (task.state, to),
            (
                TaskState::Running | TaskState::WaitingApproval,
                TaskState::Paused
            ) | (
                TaskState::Ready
                    | TaskState::Running
                    | TaskState::Paused
                    | TaskState::WaitingApproval,
                TaskState::Stopped
            )
        );
        if !allowed {
            return Err(ComputerUseError::InvalidTransition);
        }
        task.state = to;
        task.updated_at_ms = now_ms;
        Ok(())
    }

    fn require_lease(
        &self,
        id: &TaskId,
        generation: u64,
        now_ms: u64,
    ) -> Result<(), ComputerUseError> {
        let lease = self.lease.as_ref().ok_or(ComputerUseError::StaleLease)?;
        if lease.task != *id || lease.generation != generation || lease.expires_at_ms <= now_ms {
            return Err(ComputerUseError::StaleLease);
        }
        Ok(())
    }

    fn expire_lease(&mut self, now_ms: u64) {
        let expired = self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at_ms <= now_ms);
        if expired {
            if let Some(id) = self.lease.as_ref().map(|lease| lease.task.clone())
                && let Some(task) = self.tasks.get_mut(&id)
            {
                task.state = TaskState::Ready;
                task.updated_at_ms = now_ms;
            }
            self.lease = None;
        }
    }

    fn revoke_lease_for(&mut self, id: &TaskId) {
        if self.lease.as_ref().is_some_and(|lease| lease.task == *id) {
            self.lease = None;
        }
    }

    fn approval_valid(
        &self,
        proof: Option<&str>,
        task: &TaskId,
        kind: Option<ApprovalKind>,
        now_ms: u64,
    ) -> bool {
        let Some(proof) = proof else {
            return false;
        };
        let Some((approved_task, approved_kind, expires_at_ms)) = self.granted_approvals.get(proof)
        else {
            return false;
        };
        approved_task == task && Some(*approved_kind) == kind && *expires_at_ms > now_ms
    }
}

/// Provider-free adapter used by contract tests and future wire adapters.
#[derive(Debug, Default)]
pub struct FakeComputerUseAdapter {
    pub supervisor: ComputerUseSupervisor,
    pub accepted_batches: Vec<ComputerActionBatch>,
}

impl FakeComputerUseAdapter {
    pub fn submit(
        &mut self,
        batch: ComputerActionBatch,
        now_ms: u64,
    ) -> Result<u64, ComputerUseError> {
        let sequence = self.supervisor.commit_batch(&batch, now_ms)?;
        self.accepted_batches.push(batch);
        Ok(sequence)
    }
}
