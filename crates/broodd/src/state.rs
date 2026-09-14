#![allow(
    clippy::allow_attributes_without_reason,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::filter_map_next,
    clippy::iter_on_single_items,
    clippy::let_underscore_must_use,
    clippy::match_same_arms,
    clippy::needless_pass_by_ref_mut,
    clippy::option_if_let_else,
    clippy::question_mark,
    clippy::significant_drop_tightening,
    clippy::struct_field_names,
    clippy::unnecessary_debug_formatting,
    clippy::unused_self,
    clippy::unwrap_used,
    clippy::useless_let_if_seq,
    reason = "remaining suppressions cover checked geometry conversions, protocol-owned Smithay resources, and experimental state policies tracked by dedicated issues"
)]
mod package_daemon;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use brood_protocol::{BackendActorRealm, SessionIsolationStatus, managed_resize_epoch_is_valid};
#[cfg(feature = "shm-fallback")]
use smithay::wayland::shm::ShmState;
use smithay::{
    backend::allocator::dmabuf::{Dmabuf, WeakDmabuf},
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState, keyboard::XkbConfig},
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale as OutputScale, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, channel,
            generic::Generic,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_output, wl_surface::WlSurface},
        },
    },
    utils::{IsAlive, Logical, Point, SERIAL_COUNTER, Serial, Size, Transform},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        dmabuf::{DmabufFeedback, DmabufGlobal, DmabufState},
        drm_syncobj::DrmSyncobjState,
        fractional_scale::{self, FractionalScaleManagerState},
        output::OutputManagerState,
        selection::{
            data_device::DataDeviceState,
            ext_data_control::DataControlState as ExtDataControlState,
            wlr_data_control::DataControlState as WlrDataControlState,
        },
        shell::xdg::{
            SurfaceCachedState, XdgShellState, XdgToplevelSurfaceData,
            decoration::XdgDecorationState,
        },
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
    },
};
use tracing::{debug, info, warn};

use crate::{
    BROOD_HOST_DBUS_ADDRESS_ENV, BROOD_PANE_ID_ENV, BROOD_SESSION_ENV, BROODD_DBUS_ADDRESS_ENV,
    BROODD_DBUS_DESTINATION_ENV,
    actor_startup::StartupMessages,
    browser::{BrowserInstanceSummary, BrowserLaunchOptions, BrowserManager},
    clipboard::HostClipboardBridge,
    command::{SurfaceCommand, WorkspaceCommand},
    geometry::{WindowSizeNegotiation, negotiate_window_size},
    plugin_runtime::{
        PluginActorSummary, PluginHostEvent, PluginHostMessage, PluginMailboxSummary, PluginRuntime,
    },
    scheduling_domain::{
        ActorProgressMetrics, BACKEND_ACTOR_EVENTS_PER_ACTOR, BACKEND_ACTOR_EVENTS_PER_TURN,
        BACKEND_LIFECYCLE_CAPACITY, BoundedWorkMetrics, CallbackMetrics, LifecycleAdmission,
        bounded_round_robin_plan, observed_actor_event_backlog, try_admit_lifecycle_request_id,
    },
    session_bus::{SessionBusHandle, SessionBusRealm},
    state_services::{FocusService, SurfaceSessionService, TestHarnessService},
    wasm_test::service_request,
};
use brood_wasm::{TestHarnessCompletion, TestHarnessOutcomeKind, TestHarnessRuntime};

#[cfg(feature = "shm-fallback")]
const MANDATORY_SHM_FORMATS: usize = 2;
const SESSION_INIT_ACTOR_PREFIX: &str = "brood.session-init";
const BACKEND_INIT_ACTOR_ID: &str = "brood.backend-init";
pub const SESSION_OUTPUT_REFRESH_MILLIHZ: i32 = 60_000;
const BROOD_SURFACE_SESSION_ID_ENV: &str = "BROOD_SURFACE_SESSION_ID";
const SESSION_OUTPUT_SIZE: (i32, i32) = (1280, 800);
const SESSION_NAME_ADJECTIVES: &[&str] = &[
    "amber", "brisk", "calm", "dusk", "eager", "fern", "glad", "hazel", "ivory", "jolly", "keen",
    "lucid", "mellow", "nimble", "olive", "plain", "quiet", "radiant", "steady", "tidy", "umber",
    "vivid", "warm", "zephyr", "bright", "cobalt", "drift", "ember", "frost", "golden", "hardy",
    "swift",
];
const SESSION_NAME_NOUNS: &[&str] = &[
    "arch", "bay", "cove", "delta", "field", "grove", "hill", "inlet", "junction", "knoll",
    "ledge", "meadow", "notch", "oak", "pass", "quay", "ridge", "stream", "trail", "vale", "wharf",
    "yard", "zone", "basin", "bridge", "cairn", "dune", "harbor", "island", "lantern", "mound",
    "path",
];
const SESSION_NAME_ACTIONS: &[&str] = &[
    "bind", "carry", "drift", "gather", "glide", "hover", "join", "kindle", "lift", "map", "note",
    "open", "pulse", "quiet", "rise", "settle", "shine", "shift", "span", "trace", "turn",
    "unfold", "vault", "weave", "anchor", "bloom", "cross", "dwell", "echo", "forge", "hinge",
    "ripple",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    Next,
    Previous,
}

struct HostedChild {
    command: String,
    child: Option<Child>,
    pid: u32,
    pane_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostedChildToken(u64);

impl HostedChildToken {
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedChildStatus {
    Launching,
    Running,
    Exited { success: bool, detail: String },
    Unknown,
}

const HOSTED_CHILD_COMPLETION_HISTORY: usize = 256;

struct HostedChildStarted {
    pid: u32,
    command: String,
    pane_id: u64,
    session: SurfaceSessionId,
}

struct HostedChildSpawnFailure {
    command: String,
    pane_id: u64,
    error: String,
}

struct HostedChildExit {
    pid: u32,
    command: String,
    pane_id: Option<u64>,
    result: std::io::Result<ExitStatus>,
}

enum HostedChildEvent {
    Started(HostedChildStarted),
    SpawnFailed(HostedChildSpawnFailure),
    Exited(HostedChildExit),
}

fn record_hosted_child_exit_status(
    hosted_child_exits: &mut u64,
    hosted_child_failures: &mut u64,
    hosted_child_last_status: &mut Option<String>,
    command: &str,
    pane_id: Option<u64>,
    status: ExitStatus,
) {
    *hosted_child_exits += 1;
    if !status.success() {
        *hosted_child_failures += 1;
    }
    let detail = match status.code() {
        Some(code) => format!("exit:{code}"),
        None => "signal".to_string(),
    };
    *hosted_child_last_status = Some(format!("{command}:{detail}"));
    info!(
        command,
        pane_id = ?pane_id,
        status = %detail,
        success = status.success(),
        "hosted Wayland app exited"
    );
}

fn record_hosted_child_exit_error(
    hosted_child_exits: &mut u64,
    hosted_child_failures: &mut u64,
    hosted_child_last_status: &mut Option<String>,
    command: &str,
    pane_id: Option<u64>,
    err: &std::io::Error,
    status: &str,
) {
    *hosted_child_exits += 1;
    *hosted_child_failures += 1;
    *hosted_child_last_status = Some(format!("{command}:{status}"));
    warn!(
        ?err,
        command,
        pane_id = ?pane_id,
        "failed to poll hosted Wayland app"
    );
}

fn remember_hosted_child_completion(
    completions: &mut VecDeque<(HostedChildToken, bool, String)>,
    pane_id: Option<u64>,
    success: bool,
    detail: String,
) {
    let Some(pane_id) = pane_id else {
        return;
    };
    completions.push_back((HostedChildToken(pane_id), success, detail));
    while completions.len() > HOSTED_CHILD_COMPLETION_HISTORY {
        completions.pop_front();
    }
}

fn hosted_child_exit_detail(status: ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| "signal".to_string(), |code| format!("exit:{code}"))
}

fn set_fractional_scale_for_surface(surface: &WlSurface, scale: f64) {
    with_states(surface, |states| {
        fractional_scale::with_fractional_scale(states, |fractional_scale| {
            fractional_scale.set_preferred_scale(scale);
        });
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingChildPane {
    pid: Option<u32>,
    pane_id: u64,
    session: SurfaceSessionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredToplevelPane {
    pane_id: u64,
    session: SurfaceSessionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct SurfaceSessionId(u64);

impl SurfaceSessionId {
    pub const NONE: Self = Self(0);

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct SurfaceSessionSummary {
    pub id: u64,
    pub name: String,
    pub active: bool,
    pub windows: usize,
    #[serde(default)]
    pub working_directory: Option<String>,
    pub project_id: String,
    pub project_name: String,
    pub project_directory: String,
    pub branch: Option<String>,
    pub worktree: bool,
    pub git: bool,
    #[serde(default)]
    pub isolation: SessionIsolationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationWindowSummary {
    pub pane: u64,
    pub session: u64,
    pub app_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TerminalSummary {
    pub session: u64,
    pub ordinal: u64,
    pub actor: String,
}

pub type ExtensionMetadata = BTreeMap<String, String>;
pub const BROOD_STATUS_PUBLISHER: &str = "brood";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSummary {
    pub id: String,
    pub role: String,
    pub sender: String,
    pub metadata: ExtensionMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantSummary {
    pub id: String,
    pub extension_id: String,
    pub kind: String,
    pub sender: String,
    pub active: bool,
    pub detail: ExtensionMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentSummary {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub sender: String,
    pub session: u64,
    pub attention: bool,
    pub unread_notifications: u64,
    pub notification_count: u64,
    pub last_notification_title: Option<String>,
    pub last_notification_body: Option<String>,
    pub metadata: ExtensionMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StatusItemSummary {
    pub channel: String,
    pub state: String,
    pub label: String,
    pub publisher: String,
    pub metadata: ExtensionMetadata,
}

#[derive(Default)]
struct RegistryService {
    extensions: Vec<ExtensionRecord>,
    windows: Vec<WindowRecord>,
    active_window_roles: BTreeMap<String, ActiveWindowRole>,
    grants: Vec<GrantRecord>,
    agents: Vec<AgentRecord>,
    notifications: Vec<AgentNotificationRecord>,
    status_publishers: Vec<StatusPublisherRecord>,
    status_items: Vec<StatusItemRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtensionRecord {
    id: String,
    role: String,
    metadata: ExtensionMetadata,
    sender: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowRecord {
    id: String,
    extension_id: String,
    identity: ExtensionMetadata,
    role: String,
    sender: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveWindowRole {
    window_id: String,
    extension_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrantRecord {
    id: String,
    extension_id: String,
    kind: String,
    detail: ExtensionMetadata,
    sender: String,
    active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentRecord {
    id: String,
    kind: String,
    status: String,
    metadata: ExtensionMetadata,
    sender: String,
    session: SurfaceSessionId,
    attention: bool,
    unread_notifications: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentNotificationRecord {
    id: String,
    agent_id: String,
    title: String,
    subtitle: String,
    body: String,
    level: String,
    metadata: ExtensionMetadata,
    unread: bool,
}

pub struct AgentNotification {
    pub agent_id: String,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub level: String,
    pub metadata: ExtensionMetadata,
    pub sender: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusPublisherRecord {
    id: String,
    role: String,
    metadata: ExtensionMetadata,
    sender: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusItemRecord {
    channel: String,
    state: String,
    label: String,
    publisher: String,
    metadata: ExtensionMetadata,
}

impl RegistryService {
    fn register_agent(
        &mut self,
        kind: &str,
        metadata: ExtensionMetadata,
        sender: String,
        session: SurfaceSessionId,
    ) -> Result<String> {
        let kind = normalize_registry_value(kind).context("agent kind must not be empty")?;
        ensure_metadata(&metadata)?;
        let id = stable_agent_registry_id(&kind, &metadata);
        let status = metadata
            .get("status")
            .and_then(|value| normalize_registry_value(value));
        if let Some(record) = self.agents.iter_mut().find(|record| record.id == id) {
            record.kind = kind;
            if let Some(status) = status {
                record.status = status;
            }
            merge_agent_metadata(&mut record.metadata, metadata);
            record.sender = sender;
            record.session = session;
        } else {
            self.agents.push(AgentRecord {
                id: id.clone(),
                kind,
                status: status.unwrap_or_else(|| "unknown".to_string()),
                metadata,
                sender,
                session,
                attention: false,
                unread_notifications: 0,
            });
        }
        Ok(id)
    }

    fn update_agent(
        &mut self,
        agent_id: &str,
        status: &str,
        metadata: ExtensionMetadata,
        sender: String,
    ) -> Result<bool> {
        let status = normalize_registry_value(status).context("agent status must not be empty")?;
        ensure_metadata(&metadata)?;
        let Some(record) = self.agents.iter_mut().find(|record| record.id == agent_id) else {
            return Ok(false);
        };
        record.status = status;
        merge_agent_metadata(&mut record.metadata, metadata);
        record.sender = sender;
        Ok(true)
    }

    fn notify_agent(&mut self, request: AgentNotification) -> Result<String> {
        let title = normalize_registry_value(&request.title)
            .context("notification title must not be empty")?;
        let level = normalize_registry_value(&request.level).unwrap_or_else(|| "info".to_string());
        ensure_metadata(&request.metadata)?;
        let Some(record) = self
            .agents
            .iter_mut()
            .find(|record| record.id == request.agent_id)
        else {
            anyhow::bail!("unknown agent {}", request.agent_id);
        };
        record.sender = request.sender;
        record.attention = true;
        record.unread_notifications = record.unread_notifications.saturating_add(1);
        if matches!(level.as_str(), "waiting" | "blocked" | "error") {
            record.status.clone_from(&level);
        }

        let mut id_metadata = request.metadata.clone();
        id_metadata.insert("agent_id".to_string(), request.agent_id.clone());
        id_metadata.insert("title".to_string(), title.clone());
        id_metadata.insert("body".to_string(), request.body.clone());
        let id = stable_registry_id("agent-notification", [&level], &id_metadata);
        self.notifications.push(AgentNotificationRecord {
            id: id.clone(),
            agent_id: request.agent_id,
            title,
            subtitle: request.subtitle,
            body: request.body,
            level,
            metadata: request.metadata,
            unread: true,
        });
        Ok(id)
    }

    fn clear_agent_attention(&mut self, agent_id: &str) -> bool {
        let Some(record) = self.agents.iter_mut().find(|record| record.id == agent_id) else {
            return false;
        };
        record.attention = false;
        record.unread_notifications = 0;
        for notification in self
            .notifications
            .iter_mut()
            .filter(|notification| notification.agent_id == agent_id)
        {
            notification.unread = false;
        }
        true
    }

    fn unregister_agent(&mut self, agent_id: &str) -> bool {
        let before = self.agents.len();
        self.agents.retain(|agent| agent.id != agent_id);
        if before == self.agents.len() {
            return false;
        }
        self.notifications
            .retain(|notification| notification.agent_id != agent_id);
        true
    }

    fn unregister_agents_for_pane(&mut self, pane_id: u64) -> Vec<String> {
        let pane_id = pane_id.to_string();
        let removed = self
            .agents
            .iter()
            .filter(|agent| agent.metadata.get("pane_id") == Some(&pane_id))
            .map(|agent| agent.id.clone())
            .collect::<BTreeSet<_>>();
        if removed.is_empty() {
            return Vec::new();
        }
        self.agents.retain(|agent| !removed.contains(&agent.id));
        self.notifications
            .retain(|notification| !removed.contains(&notification.agent_id));
        removed.into_iter().collect()
    }
}

#[derive(Clone)]
struct HostedPluginFilesystem {
    host_path: PathBuf,
    writable: bool,
}

struct RealmInitState {
    actor_id: String,
    root: PathBuf,
    children: BTreeMap<String, String>,
    apps: BTreeMap<String, HostedChildToken>,
}

struct PendingRealmArtifact {
    session: SurfaceSessionId,
    id: String,
    path: String,
    capabilities: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("realm package member is missing: {path}")]
struct MissingRealmArtifact {
    path: String,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct ManagedWorktreeRecord {
    project_id: String,
    project_name: String,
    source_directory: String,
    branch: String,
    path: String,
    metadata_path: String,
    session: Option<u64>,
    last_used_unix: u64,
}

struct PendingManagedWorktree {
    project: brood_cli::project::ProjectContext,
    branch: String,
    target: PathBuf,
    response: ManagedWorktreeResponse,
}

enum ManagedWorktreeResponse {
    Control(tokio::sync::oneshot::Sender<std::result::Result<u64, String>>),
    Presentation { actor: String, request: u64 },
}

#[derive(serde::Deserialize)]
struct BackendActorHostMessage {
    #[serde(rename = "type")]
    message_type: String,
    request: Option<u64>,
    path: Option<String>,
    message: Option<String>,
    source_session: Option<u64>,
    branch: Option<String>,
    presentation_actor: Option<String>,
}

enum BackendLifecycleCompletion {
    HostedReplacementReady {
        request: u64,
        actor_id: String,
        path: PathBuf,
        purpose: HostedReplacementPurpose,
        lifecycle: Option<ConfiguredPluginLifecycle>,
        result: std::result::Result<Box<brood_wasm::HostedWasmActor>, String>,
    },
    LegacyReplacementReady {
        request: u64,
        path: PathBuf,
        generation: Option<brood_wasm::LegacyWasmActorGeneration>,
        result: std::result::Result<(), String>,
    },
    RealmChildStopped {
        session: SurfaceSessionId,
        id: String,
        result: std::result::Result<(), String>,
    },
    ActorRetired {
        actor_id: String,
        result: std::result::Result<(), String>,
    },
}

#[derive(Clone, Debug)]
enum HostedReplacementPurpose {
    Generic,
    DaemonChild {
        id: String,
    },
    RealmRoot {
        session: SurfaceSessionId,
    },
    RealmChild {
        session: SurfaceSessionId,
        id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConfiguredWorkspacePlugin {
    path: PathBuf,
    writable: bool,
    startup_message: Option<String>,
    lifecycle: Option<ConfiguredPluginLifecycle>,
    resources: ConfiguredPluginResources,
    runtime_io: brood_wasm::RuntimeIoCapabilities,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfiguredPluginResources {
    pub filesystem_watches: usize,
    pub filesystem_watch_events: usize,
}

impl Default for ConfiguredPluginResources {
    fn default() -> Self {
        Self {
            filesystem_watches: 16,
            filesystem_watch_events: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeToolAccess {
    None,
    ComputerUse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredPluginLifecycle {
    pub start_timeout: Duration,
    pub stop_timeout: Duration,
    pub data: BTreeMap<String, String>,
}

fn wasm_plugin_instance_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("plugin")
        .to_string()
}

fn ensure_legacy_instance_id_supported(instance_id: Option<&str>) -> Result<()> {
    ensure!(
        instance_id.is_none(),
        "custom instance IDs are unsupported for legacy core WASM plugins; \
         they register their declared actor export names and replace the singular legacy lane"
    );
    Ok(())
}

const fn legacy_instance_id_to_validate(
    instance_id: Option<&str>,
    allow_config_label: bool,
) -> Option<&str> {
    if allow_config_label {
        None
    } else {
        instance_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentationConfigurePolicy {
    Deduplicate,
    Force,
}

impl PresentationConfigurePolicy {
    const fn should_emit(self, already_configured: bool) -> bool {
        matches!(self, Self::Force) || !already_configured
    }
}

fn emit_presentation_toplevel_configure(
    toplevel: &smithay::wayland::shell::xdg::ToplevelSurface,
    size: Size<i32, Logical>,
    policy: PresentationConfigurePolicy,
) -> Option<Serial> {
    toplevel.with_pending_state(|state| apply_presentation_toplevel_state(state, size));
    match policy {
        PresentationConfigurePolicy::Deduplicate => toplevel.send_pending_configure(),
        PresentationConfigurePolicy::Force => Some(toplevel.send_configure()),
    }
}

fn apply_presentation_toplevel_state(
    state: &mut smithay::wayland::shell::xdg::ToplevelState,
    size: Size<i32, Logical>,
) {
    state.size = Some(size);
    state.bounds = Some(size);
    for tiled in [
        xdg_toplevel::State::TiledLeft,
        xdg_toplevel::State::TiledRight,
        xdg_toplevel::State::TiledTop,
        xdg_toplevel::State::TiledBottom,
    ] {
        state.states.set(tiled);
    }
}

fn presentation_toplevel_state_matches(
    state: &smithay::wayland::shell::xdg::ToplevelState,
    size: Size<i32, Logical>,
) -> bool {
    state.size == Some(size)
        && state.bounds == Some(size)
        && [
            xdg_toplevel::State::TiledLeft,
            xdg_toplevel::State::TiledRight,
            xdg_toplevel::State::TiledTop,
            xdg_toplevel::State::TiledBottom,
        ]
        .into_iter()
        .all(|tiled| state.states.contains(tiled))
}

pub struct Brood {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Self>,
    pub loop_signal: LoopSignal,

    pub(crate) session_output: Output,
    pub space: Space<Window>,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    #[cfg(feature = "shm-fallback")]
    pub shm_state: ShmState,
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    #[allow(dead_code)]
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub ext_data_control_state: ExtDataControlState,
    pub wlr_data_control_state: WlrDataControlState,
    pub(crate) host_clipboard_bridge: Option<HostClipboardBridge>,
    #[allow(dead_code)]
    pub dmabuf: DmabufRuntime,
    pub popups: PopupManager,
    pub seat: Seat<Self>,

    focus: FocusService,
    pub browsers: BrowserManager,
    pub(crate) computer_use: brood_computer_use::ComputerUseSupervisor,
    pub(crate) computer_use_automation: crate::computer_use::AutomationRuntime,
    pub stats: RuntimeStats,
    next_window_slot: usize,
    last_window_count: usize,
    window_lifecycle_refresh_pending: bool,
    fullscreen_toplevels: Vec<(WlSurface, Option<wl_output::WlOutput>)>,
    next_pane_id: u64,
    window_pane_ids: Vec<(WlSurface, u64)>,
    presentation_client_requested_sizes: BTreeMap<u64, (i32, i32)>,
    known_surfaces: Vec<WlSurface>,
    pending_child_panes: Vec<PendingChildPane>,
    pub(crate) ctrl_forwarded_to_client: bool,
    pub(crate) alt_forwarded_to_client: bool,
    pub(crate) shift_forwarded_to_client: bool,
    pub(crate) forwarded_pressed_keys: Vec<smithay::backend::input::Keycode>,
    sessions: SurfaceSessionService,
    extension_registry: RegistryService,
    plugin_runtime: Option<PluginRuntime>,
    plugin_host_event_sender: channel::Sender<PluginHostEvent>,
    plugin_actor_messages: u64,
    plugin_actor_failures: u64,
    plugin_actor_last_status: Option<String>,
    wasm_registry: Arc<tokio::sync::Mutex<brood_wasm::Registry>>,
    wasm_host_mailbox: Option<brood_wasm::Mailbox>,
    legacy_wasm_actor_generation: Option<brood_wasm::LegacyWasmActorGeneration>,
    wasm_io_actors: BTreeMap<String, brood_wasm::HostedWasmActor>,
    backend_actor_drain_cursor: usize,
    backend_actor_progress: ActorProgressMetrics,
    backend_lifecycle_completion_sender: tokio::sync::mpsc::Sender<BackendLifecycleCompletion>,
    backend_lifecycle_completions: tokio::sync::mpsc::Receiver<BackendLifecycleCompletion>,
    backend_lifecycle_tasks: Vec<tokio::task::JoinHandle<()>>,
    backend_lifecycle_admission: LifecycleAdmission,
    event_loop_active: bool,
    next_hosted_replacement_request: u64,
    pending_hosted_replacements: BTreeMap<String, u64>,
    pending_hosted_startup_messages: StartupMessages,
    pending_legacy_replacement: Option<u64>,
    pending_legacy_actor_names: BTreeSet<String>,
    starting_realm_children: BTreeSet<(SurfaceSessionId, String)>,
    starting_daemon_children: BTreeSet<String>,
    stop_daemon_while_starting: BTreeSet<String>,
    stopping_daemon_children: BTreeMap<String, String>,
    stop_requested_while_starting: BTreeSet<(SurfaceSessionId, String)>,
    backend_init_actor: Option<PathBuf>,
    backend_init: Option<RealmInitState>,
    next_managed_worktree_request: u64,
    pending_managed_worktrees: BTreeMap<u64, PendingManagedWorktree>,
    session_init_actor: Option<PathBuf>,
    realm_inits: BTreeMap<SurfaceSessionId, RealmInitState>,
    package_host: brood_cli::package_runtime::PackageHost,
    pending_realm_artifacts: BTreeMap<u64, PendingRealmArtifact>,
    queued_realm_artifacts: VecDeque<(u64, String)>,
    next_realm_artifact_request: u64,
    configured_workspace_plugins: BTreeMap<String, ConfiguredWorkspacePlugin>,
    configured_plugin_lifecycles: BTreeMap<String, ConfiguredPluginLifecycle>,
    configured_plugin_resources: BTreeMap<String, ConfiguredPluginResources>,
    configured_plugin_runtime_io: BTreeMap<String, brood_wasm::RuntimeIoCapabilities>,
    filesystem_watcher_service: brood_wasm::NativeFilesystemWatcherService,
    pub(crate) test_surface_markers: bool,
    pub(crate) test_harness: TestHarnessService,
    pub(crate) brood_session: Option<String>,
    pub(crate) host_dbus_address: Option<String>,
    pub(crate) brood_dbus_destination: Option<String>,
    pub(crate) brood_dbus_address: Option<String>,
    pub(crate) brood_native_tool_socket: Option<OsString>,
    hosted_children: Vec<HostedChild>,
    hosted_child_event_sender: channel::Sender<HostedChildEvent>,
    async_runtime: Option<tokio::runtime::Handle>,
    actor_runtime: Option<tokio::runtime::Handle>,
    hosted_child_exits: u64,
    hosted_child_failures: u64,
    hosted_child_last_status: Option<String>,
    hosted_child_completions: VecDeque<(HostedChildToken, bool, String)>,
}

impl Brood {
    pub fn new(event_loop: &mut EventLoop<'static, Self>, display: Display<Self>) -> Self {
        let start_time = Instant::now();
        let display_handle = display.handle();
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        #[cfg(feature = "shm-fallback")]
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let fractional_scale_manager_state =
            FractionalScaleManagerState::new::<Self>(&display_handle);
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let ext_data_control_state =
            ExtDataControlState::new::<Self, _>(&display_handle, None, |_| true);
        let wlr_data_control_state =
            WlrDataControlState::new::<Self, _>(&display_handle, None, |_| true);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "brood");
        seat.add_keyboard(XkbConfig::default(), 200, 25)
            .expect("keyboard capability");
        seat.add_pointer();

        let session_output = Self::init_session_output(&display_handle);
        let mut space = Space::default();
        space.map_output(&session_output, (0, 0));

        let socket_name = Self::init_wayland_listener(display, event_loop);
        let hosted_child_event_sender = Self::init_hosted_child_event_source(event_loop);
        let host_clipboard_bridge = Self::init_host_clipboard_bridge(event_loop);
        let plugin_host_event_sender = Self::init_plugin_host_event_source(event_loop);
        let loop_handle = event_loop.handle();
        let loop_signal = event_loop.get_signal();

        let dmabuf = DmabufRuntime {
            #[cfg(feature = "shm-fallback")]
            shm_formats: MANDATORY_SHM_FORMATS,
            ..DmabufRuntime::default()
        };
        let mut wasm_registry = brood_wasm::Registry::new();
        let wasm_host_mailbox = wasm_registry
            .register_host_mailbox(256)
            .expect("register WASM host mailbox");
        let (backend_lifecycle_completion_sender, backend_lifecycle_completions) =
            tokio::sync::mpsc::channel(BACKEND_LIFECYCLE_CAPACITY);

        let package_signal = loop_signal.clone();
        let package_host =
            brood_cli::package_runtime::PackageHost::new(move || package_signal.wakeup());

        Self {
            start_time,
            socket_name,
            display_handle,
            loop_handle,
            loop_signal,
            session_output,
            space,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            #[cfg(feature = "shm-fallback")]
            shm_state,
            output_manager_state,
            fractional_scale_manager_state,
            viewporter_state,
            seat_state,
            data_device_state,
            ext_data_control_state,
            wlr_data_control_state,
            host_clipboard_bridge,
            dmabuf,
            popups: PopupManager::default(),
            seat,
            focus: FocusService::default(),
            browsers: BrowserManager::default(),
            computer_use: brood_computer_use::ComputerUseSupervisor::new(),
            computer_use_automation: crate::computer_use::AutomationRuntime::default(),
            stats: RuntimeStats::default(),
            next_window_slot: 0,
            last_window_count: 0,
            window_lifecycle_refresh_pending: false,
            fullscreen_toplevels: Vec::new(),
            next_pane_id: 1,
            window_pane_ids: Vec::new(),
            presentation_client_requested_sizes: BTreeMap::new(),
            known_surfaces: Vec::new(),
            pending_child_panes: Vec::new(),
            ctrl_forwarded_to_client: false,
            alt_forwarded_to_client: false,
            shift_forwarded_to_client: false,
            forwarded_pressed_keys: Vec::new(),
            sessions: SurfaceSessionService::new(
                current_process_directory(),
                default_surface_session_name(SurfaceSessionId(1)),
            ),
            extension_registry: RegistryService::default(),
            plugin_runtime: None,
            plugin_host_event_sender,
            plugin_actor_messages: 0,
            plugin_actor_failures: 0,
            plugin_actor_last_status: None,
            wasm_registry: Arc::new(tokio::sync::Mutex::new(wasm_registry)),
            wasm_host_mailbox: Some(wasm_host_mailbox),
            legacy_wasm_actor_generation: None,
            wasm_io_actors: BTreeMap::new(),
            backend_actor_drain_cursor: 0,
            backend_actor_progress: ActorProgressMetrics::default(),
            backend_lifecycle_completion_sender,
            backend_lifecycle_completions,
            backend_lifecycle_tasks: Vec::with_capacity(BACKEND_LIFECYCLE_CAPACITY),
            backend_lifecycle_admission: LifecycleAdmission::default(),
            event_loop_active: false,
            next_hosted_replacement_request: 1,
            pending_hosted_replacements: BTreeMap::new(),
            pending_hosted_startup_messages: StartupMessages::default(),
            pending_legacy_replacement: None,
            pending_legacy_actor_names: BTreeSet::new(),
            starting_realm_children: BTreeSet::new(),
            starting_daemon_children: BTreeSet::new(),
            stop_daemon_while_starting: BTreeSet::new(),
            stopping_daemon_children: BTreeMap::new(),
            stop_requested_while_starting: BTreeSet::new(),
            backend_init_actor: None,
            backend_init: None,
            next_managed_worktree_request: 1,
            pending_managed_worktrees: BTreeMap::new(),
            session_init_actor: None,
            realm_inits: BTreeMap::new(),
            package_host,
            pending_realm_artifacts: BTreeMap::new(),
            queued_realm_artifacts: VecDeque::new(),
            next_realm_artifact_request: 1,
            configured_workspace_plugins: BTreeMap::new(),
            configured_plugin_lifecycles: BTreeMap::new(),
            configured_plugin_resources: BTreeMap::new(),
            configured_plugin_runtime_io: BTreeMap::new(),
            filesystem_watcher_service: brood_wasm::NativeFilesystemWatcherService::new(),
            test_surface_markers: false,
            test_harness: TestHarnessService::default(),
            brood_session: None,
            host_dbus_address: std::env::var(BROOD_HOST_DBUS_ADDRESS_ENV)
                .ok()
                .or_else(|| std::env::var("DBUS_SESSION_BUS_ADDRESS").ok()),
            brood_dbus_destination: None,
            brood_dbus_address: None,
            brood_native_tool_socket: None,
            hosted_children: Vec::new(),
            hosted_child_event_sender,
            async_runtime: None,
            actor_runtime: None,
            hosted_child_exits: 0,
            hosted_child_failures: 0,
            hosted_child_last_status: None,
            hosted_child_completions: VecDeque::new(),
        }
    }

    pub fn set_runtime_domains(
        &mut self,
        general_runtime: tokio::runtime::Handle,
        actor_runtime: tokio::runtime::Handle,
    ) {
        if self.plugin_runtime.is_none() {
            self.plugin_runtime = Some(PluginRuntime::start_builtin(
                &actor_runtime,
                self.plugin_host_event_sender.clone(),
            ));
            self.plugin_actor_last_status = Some("builtin:started".to_string());
        }
        self.async_runtime = Some(general_runtime);
        self.actor_runtime = Some(actor_runtime);
    }

    /// Mark the point after startup configuration where the calloop owner may
    /// no longer synchronously wait for backend actor work.
    pub(crate) const fn begin_event_loop(&mut self) {
        self.event_loop_active = true;
    }

    /// Mark the point after calloop has stopped. Ordered shutdown may use the
    /// blocking join helpers because no latency-sensitive owner callback can
    /// run after this point.
    pub(crate) const fn end_event_loop(&mut self) {
        self.event_loop_active = false;
    }

    pub fn send_plugin_actor_message(
        &mut self,
        actor_id: &str,
        sender: String,
        body: String,
    ) -> Result<()> {
        self.drain_hosted_actor_events();
        if self.wasm_io_actors.contains_key(actor_id)
            || self.pending_hosted_replacements.contains_key(actor_id)
        {
            self.try_route_wasm_actor_message(actor_id, sender, body.into_bytes())?;
            self.plugin_actor_messages += 1;
            self.plugin_actor_last_status = Some(format!("{actor_id}:message_queued"));
            return Ok(());
        }
        if self
            .legacy_wasm_actor_generation
            .as_ref()
            .is_some_and(|generation| generation.contains_actor(actor_id))
            || self.pending_legacy_actor_names.contains(actor_id)
        {
            self.try_route_wasm_actor_message(actor_id, sender, body.into_bytes())?;
            self.plugin_actor_messages += 1;
            self.plugin_actor_last_status = Some(format!("{actor_id}:message_queued"));
            return Ok(());
        }
        let plugin_runtime = self
            .plugin_runtime
            .as_mut()
            .context("plugin runtime is not running")?;
        plugin_runtime.send_message(actor_id, sender, body)?;
        self.plugin_actor_messages += 1;
        self.plugin_actor_last_status = Some(format!("{actor_id}:message_queued"));
        self.drain_plugin_host_mailbox();
        Ok(())
    }

    /// Deliver one presentation-originated mailbox message to an actor owned
    /// by the explicitly named surface-session realm.
    pub(crate) fn send_session_actor_message(
        &mut self,
        session: SurfaceSessionId,
        actor: &str,
        sender: &str,
        body: Vec<u8>,
    ) -> Result<()> {
        self.drain_hosted_actor_events();
        ensure!(
            self.sessions.contains(session),
            "unknown surface session {}",
            session.raw()
        );
        let init = self
            .realm_inits
            .get(&session)
            .context("surface session has no actor realm")?;
        let actor_id = if actor == "init" || actor == init.actor_id {
            init.actor_id.clone()
        } else if let Some(actor_id) = init.children.get(actor) {
            actor_id.clone()
        } else if init.children.values().any(|actor_id| actor_id == actor) {
            actor.to_string()
        } else {
            anyhow::bail!(
                "actor {actor:?} does not belong to surface session {}",
                session.raw()
            );
        };
        let sender = format!("presentation:{sender}");
        self.try_route_wasm_actor_message(&actor_id, sender.clone(), body)?;
        self.plugin_actor_messages = self.plugin_actor_messages.saturating_add(1);
        self.plugin_actor_last_status = Some(format!("{actor_id}:message_queued"));
        info!(
            surface_session = session.raw(),
            actor = %actor_id,
            sender = %sender,
            "delivered presentation-to-session actor message"
        );
        Ok(())
    }

    /// Deliver one presentation-originated mailbox message to the daemon
    /// realm's blessed init actor or one of its owned children.
    pub(crate) fn send_daemon_actor_message(
        &mut self,
        actor: &str,
        sender: &str,
        body: Vec<u8>,
    ) -> Result<()> {
        self.drain_hosted_actor_events();
        let init = self
            .backend_init
            .as_ref()
            .context("daemon actor realm is unavailable")?;
        let actor_id = if actor == "init" || actor == init.actor_id {
            init.actor_id.clone()
        } else if let Some(actor_id) = init.children.get(actor) {
            actor_id.clone()
        } else if init.children.values().any(|actor_id| actor_id == actor) {
            actor.to_string()
        } else {
            anyhow::bail!("actor {actor:?} does not belong to the daemon realm")
        };
        let sender = format!("presentation:{sender}");
        self.try_route_wasm_actor_message(&actor_id, sender.clone(), body)?;
        self.plugin_actor_messages = self.plugin_actor_messages.saturating_add(1);
        self.plugin_actor_last_status = Some(format!("{actor_id}:message_queued"));
        info!(
            actor = %actor_id,
            sender = %sender,
            "delivered presentation-to-daemon actor message"
        );
        Ok(())
    }

    fn send_local_realm_actor_message(
        &mut self,
        session: SurfaceSessionId,
        actor: &str,
        sender: &str,
        body: Vec<u8>,
    ) -> Result<()> {
        let actor_id = self.resolve_realm_actor_id(session, actor)?;
        self.try_route_wasm_actor_message(&actor_id, sender.to_string(), body)?;
        self.plugin_actor_messages = self.plugin_actor_messages.saturating_add(1);
        self.plugin_actor_last_status = Some(format!("{actor_id}:message_queued"));
        Ok(())
    }

    fn resolve_realm_actor_id(&self, session: SurfaceSessionId, actor: &str) -> Result<String> {
        ensure!(
            self.sessions.contains(session),
            "unknown surface session {}",
            session.raw()
        );
        let init = self
            .realm_inits
            .get(&session)
            .context("surface session has no actor realm")?;
        if actor == "init" || actor == init.actor_id {
            Ok(init.actor_id.clone())
        } else if let Some(actor_id) = init.children.get(actor) {
            Ok(actor_id.clone())
        } else if init.children.values().any(|actor_id| actor_id == actor) {
            Ok(actor.to_string())
        } else {
            anyhow::bail!(
                "actor {actor:?} does not belong to surface session {}",
                session.raw()
            )
        }
    }

    fn send_presentation_actor_message(
        &mut self,
        realm: BackendActorRealm,
        sender: &str,
        actor: &str,
        body: &[u8],
    ) -> Result<()> {
        // A presentation owns session-scoped actors for inactive sessions too.
        // Preserve the realm on the wire; the client checks the recipient's
        // association. Visibility must not suppress inventory or state replies.
        if let BackendActorRealm::Session(session) = realm {
            ensure!(
                self.sessions.contains(SurfaceSessionId::from_raw(session)),
                "unknown surface session {session}"
            );
        }
        let fast_path = self
            .dmabuf
            .fast_path
            .as_mut()
            .context("no presentation fast path is attached")?;
        fast_path.presentation_message_sequence =
            fast_path.presentation_message_sequence.wrapping_add(1);
        let record = brood_ipc::ActorMessageRecord::new(
            fast_path.generation,
            fast_path.presentation_message_sequence,
            realm,
            sender,
            actor,
            body,
        )?;
        if !fast_path.deferred_presentation_messages.is_empty() {
            ensure!(
                fast_path.deferred_presentation_messages.len() < 64,
                "session-to-presentation actor message backlog is full"
            );
            fast_path.deferred_presentation_messages.push_back(record);
            return Ok(());
        }
        if let Err(record) = fast_path.presentation_messages.try_send(record) {
            fast_path.deferred_presentation_messages.push_back(record);
        } else {
            fast_path.presentation_message_notifications.notify()?;
        }
        info!(
            ?realm,
            sender, actor, "queued session-to-presentation actor message"
        );
        Ok(())
    }

    pub(crate) fn send_computer_use_observer_message(
        &mut self,
        recipient: &str,
        body: &[u8],
    ) -> Result<()> {
        self.send_presentation_actor_message(
            BackendActorRealm::Daemon,
            "computer-use",
            recipient,
            body,
        )
    }

    fn flush_presentation_actor_messages(&mut self) {
        let Some(fast_path) = self.dmabuf.fast_path.as_mut() else {
            return;
        };
        let mut published = false;
        while let Some(record) = fast_path.deferred_presentation_messages.pop_front() {
            if let Err(record) = fast_path.presentation_messages.try_send(record) {
                fast_path.deferred_presentation_messages.push_front(record);
                break;
            }
            published = true;
        }
        if published {
            let _ = fast_path.presentation_message_notifications.notify();
        }
    }

    pub fn send_plugin_actor_message_to_slot(
        &mut self,
        actor_id: &str,
        mailbox_slot: &str,
        sender: String,
        body: String,
    ) -> Result<()> {
        if let Some(actor) = self.wasm_io_actors.get(actor_id) {
            ensure!(
                mailbox_slot == "default",
                "{} actor {actor_id:?} has no mailbox slot {mailbox_slot:?}",
                actor.kind()
            );
            return self.send_plugin_actor_message(actor_id, sender, body);
        }
        let plugin_runtime = self
            .plugin_runtime
            .as_mut()
            .context("plugin runtime is not running")?;
        plugin_runtime.send_message_to_slot(actor_id, mailbox_slot, sender, body)?;
        self.plugin_actor_messages += 1;
        self.plugin_actor_last_status = Some(format!("{actor_id}:{mailbox_slot}:message_queued"));
        self.drain_plugin_host_mailbox();
        Ok(())
    }

    pub fn plugin_actor_summaries(&mut self) -> Vec<PluginActorSummary> {
        self.drain_hosted_actor_events();
        let mut summaries = self
            .plugin_runtime
            .as_ref()
            .map_or_else(Vec::new, PluginRuntime::actor_summaries);
        for (actor_id, actor) in &self.wasm_io_actors {
            let (enqueued_messages, dropped_messages, mailbox_capacity) = match actor {
                brood_wasm::HostedWasmActor::Component(actor) => {
                    let stats = actor.mailbox_stats();
                    (
                        stats.enqueued_messages,
                        stats.dropped_messages,
                        stats.capacity,
                    )
                }
                brood_wasm::HostedWasmActor::Preview1(actor) => {
                    let stats = actor.stats();
                    (
                        stats.mailbox_queued,
                        stats.mailbox_dropped,
                        stats.mailbox_capacity,
                    )
                }
            };
            summaries.push(PluginActorSummary {
                id: actor_id.clone(),
                kind: actor.kind().to_string(),
                enqueued_messages,
                dropped_messages,
                mailbox_capacity,
                mailbox_slots: vec![PluginMailboxSummary {
                    slot: "default".to_string(),
                    enqueued_messages,
                    dropped_messages,
                    capacity: mailbox_capacity,
                }],
                published_frames: 0,
                latest_frame: "none".to_string(),
            });
        }
        summaries
    }

    pub(crate) fn terminal_summaries(&mut self) -> Vec<TerminalSummary> {
        let mut terminals = self
            .plugin_actor_summaries()
            .into_iter()
            .filter_map(|actor| parse_terminal_actor_id(&actor.id))
            .collect::<Vec<_>>();
        terminals.sort_by_key(|terminal| (terminal.session, terminal.ordinal));
        terminals
    }

    pub(crate) fn request_terminal_create(
        &mut self,
        session: SurfaceSessionId,
        command: &str,
    ) -> Result<()> {
        ensure!(
            self.sessions.contains(session),
            "unknown surface session {}",
            session.raw()
        );
        ensure!(
            command.len() <= 8 * 1024 && !command.contains('\0'),
            "terminal command is invalid or exceeds 8192 bytes"
        );
        let body = serde_json::to_vec(&serde_json::json!({
            "kind": "create-terminal",
            "command": command,
        }))?;
        // The blessed client init actor owns creation of the matching terminal
        // presentation actor. Giving the request its identity makes the normal
        // terminal-started reply take that existing path.
        self.send_session_actor_message(session, "init", "brood.client-init", body)
    }

    pub(crate) fn send_terminal_input(
        &mut self,
        session: SurfaceSessionId,
        ordinal: u64,
        bytes: &[u8],
    ) -> Result<()> {
        let actor = terminal_actor_name(ordinal)?;
        let mut body = Vec::with_capacity(bytes.len().saturating_add(9));
        body.push(b'I');
        body.extend_from_slice(&0_u64.to_le_bytes());
        body.extend_from_slice(bytes);
        self.send_session_actor_message(session, &actor, "bmux", body)
    }

    pub(crate) fn request_terminal_focus(
        &mut self,
        session: SurfaceSessionId,
        ordinal: u64,
    ) -> Result<()> {
        ensure!(
            self.sessions.active() == session,
            "surface session {} is not active",
            session.raw()
        );
        let terminal = terminal_actor_name(ordinal)?;
        let body = serde_json::to_vec(&serde_json::json!({
            "kind": "bmux-focus-terminal",
            "terminal": terminal,
        }))?;
        self.send_presentation_actor_message(
            BackendActorRealm::Session(session.raw()),
            "init",
            "brood.client-init",
            &body,
        )
    }

    pub(crate) fn request_terminal_stop(
        &mut self,
        session: SurfaceSessionId,
        ordinal: u64,
    ) -> Result<()> {
        let terminal = terminal_actor_name(ordinal)?;
        let body = serde_json::to_vec(&serde_json::json!({
            "kind": "stop-terminal",
            "terminal": terminal,
        }))?;
        self.send_session_actor_message(session, "init", "bmux", body)?;
        if self.sessions.active() == session && self.dmabuf.fast_path.is_some() {
            let body = serde_json::to_vec(&serde_json::json!({
                "kind": "bmux-stop-terminal",
                "terminal": terminal,
            }))?;
            self.send_presentation_actor_message(
                BackendActorRealm::Session(session.raw()),
                "init",
                "brood.client-init",
                &body,
            )?;
        }
        Ok(())
    }

    pub fn plugin_mailbox_summaries(
        &mut self,
        actor_id: &str,
    ) -> Result<Vec<PluginMailboxSummary>> {
        self.drain_hosted_actor_events();
        if let Some(actor) = self.wasm_io_actors.get(actor_id) {
            let (enqueued_messages, dropped_messages, capacity) = match actor {
                brood_wasm::HostedWasmActor::Component(actor) => {
                    let stats = actor.mailbox_stats();
                    (
                        stats.enqueued_messages,
                        stats.dropped_messages,
                        stats.capacity,
                    )
                }
                brood_wasm::HostedWasmActor::Preview1(actor) => {
                    let stats = actor.stats();
                    (
                        stats.mailbox_queued,
                        stats.mailbox_dropped,
                        stats.mailbox_capacity,
                    )
                }
            };
            return Ok(vec![PluginMailboxSummary {
                slot: "default".to_string(),
                enqueued_messages,
                dropped_messages,
                capacity,
            }]);
        }
        let plugin_runtime = self
            .plugin_runtime
            .as_ref()
            .context("plugin runtime is not running")?;
        plugin_runtime.mailbox_summaries(actor_id)
    }

    pub fn preview1_compat_stats(&self) -> Option<brood_wasm::Preview1CompatStats> {
        brood_wasm::aggregate_preview1_compat_stats(self.wasm_io_actors.values().filter_map(
            |actor| match actor {
                brood_wasm::HostedWasmActor::Preview1(actor) => Some(actor.stats()),
                brood_wasm::HostedWasmActor::Component(_) => None,
            },
        ))
    }

    pub fn filesystem_watcher_stats(&self) -> brood_wasm::NativeFilesystemWatcherStats {
        self.filesystem_watcher_service.stats()
    }

    pub fn hosted_filesystem_watch_stats(&self) -> brood_wasm::FilesystemWatchStats {
        self.wasm_io_actors.values().fold(
            brood_wasm::FilesystemWatchStats::default(),
            |mut aggregate, actor| {
                let next = actor.filesystem_watch_stats();
                aggregate.watches = aggregate.watches.saturating_add(next.watches);
                aggregate.max_watches = aggregate.max_watches.saturating_add(next.max_watches);
                aggregate.max_events = aggregate.max_events.max(next.max_events);
                aggregate.overflows = aggregate.overflows.saturating_add(next.overflows);
                aggregate.rescans = aggregate.rescans.saturating_add(next.rescans);
                aggregate
            },
        )
    }

    pub fn hosted_runtime_io_stats(&self) -> brood_wasm::RuntimeIoStats {
        self.wasm_io_actors.values().fold(
            brood_wasm::RuntimeIoStats::default(),
            |mut aggregate, actor| {
                let next = actor.runtime_io_stats();
                aggregate.resources = aggregate.resources.saturating_add(next.resources);
                aggregate.streams = aggregate.streams.saturating_add(next.streams);
                aggregate.children = aggregate.children.saturating_add(next.children);
                aggregate.timers = aggregate.timers.saturating_add(next.timers);
                aggregate.buffered_incoming_bytes = aggregate
                    .buffered_incoming_bytes
                    .saturating_add(next.buffered_incoming_bytes);
                aggregate.buffered_outgoing_bytes = aggregate
                    .buffered_outgoing_bytes
                    .saturating_add(next.buffered_outgoing_bytes);
                aggregate.max_stream_incoming_bytes = aggregate
                    .max_stream_incoming_bytes
                    .max(next.max_stream_incoming_bytes);
                aggregate.max_stream_outgoing_bytes = aggregate
                    .max_stream_outgoing_bytes
                    .max(next.max_stream_outgoing_bytes);
                aggregate.stream_read_would_block = aggregate
                    .stream_read_would_block
                    .saturating_add(next.stream_read_would_block);
                aggregate.stream_write_would_block = aggregate
                    .stream_write_would_block
                    .saturating_add(next.stream_write_would_block);
                aggregate
            },
        )
    }

    /// Route a byte-oriented policy message through the shared public actor
    /// namespace. Hosted Component/Preview actors carry sender metadata in
    /// their mailbox ABI; legacy actors retain their older byte-only mailbox.
    fn try_route_wasm_actor_message(
        &mut self,
        actor_id: &str,
        sender: String,
        payload: Vec<u8>,
    ) -> Result<()> {
        if let Some(actor) = self.wasm_io_actors.get(actor_id) {
            return actor.try_send_message(sender, payload);
        }
        if self.pending_hosted_replacements.contains_key(actor_id) {
            // Session catalogues can reach the presentation before their init
            // actors finish loading. Accept ordinary messages for that admitted
            // startup and preserve their authenticated sender until it is ready.
            return self
                .pending_hosted_startup_messages
                .push(actor_id, sender, payload);
        }
        self.try_with_wasm_registry(|registry| {
            registry.try_post_named_message(actor_id, payload)
        })??;
        Ok(())
    }

    fn emit_wasm_actor_event(&mut self, event: brood_wasm::WasmActorEventKind, payload: &str) {
        let actor_names = self
            .legacy_wasm_actor_generation
            .as_ref()
            .map_or_else(Vec::new, |generation| generation.subscribers(event));
        for actor in actor_names {
            let result = self.try_with_wasm_registry(|registry| {
                registry.try_post_named_message(&actor, payload.as_bytes().to_vec())
            });
            match result {
                Ok(Ok(())) => {
                    self.plugin_actor_messages += 1;
                    self.plugin_actor_last_status = Some(format!("{actor}:event_queued"));
                }
                Ok(Err(err)) | Err(err) => {
                    self.plugin_actor_failures += 1;
                    self.plugin_actor_last_status = Some(format!("{actor}:event_error"));
                    warn!(actor = %actor, event = event.message_type(), error = %err, "failed to enqueue subscribed WASM actor event");
                }
            }
        }
    }

    fn try_with_wasm_registry<T>(
        &self,
        f: impl FnOnce(&mut brood_wasm::Registry) -> T,
    ) -> Result<T> {
        let mut registry = self
            .wasm_registry
            .try_lock()
            .context("WASM registry is busy; caller must retry")?;
        Ok(f(&mut registry))
    }

    fn drain_wasm_host_mailbox(&mut self) {
        let Some(mut mailbox) = self.wasm_host_mailbox.take() else {
            return;
        };
        loop {
            let message = match mailbox.try_read_message() {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(err) => {
                    self.plugin_actor_failures += 1;
                    self.plugin_actor_last_status = Some("wasm-host:mailbox_error".to_string());
                    warn!(?err, "WASM host mailbox failed");
                    break;
                }
            };
            self.apply_wasm_host_message(message);
        }
        self.wasm_host_mailbox = Some(mailbox);
    }

    fn apply_wasm_host_message(&mut self, message: Vec<u8>) {
        let body = match String::from_utf8(message) {
            Ok(body) => body,
            Err(err) => {
                self.plugin_actor_failures += 1;
                self.plugin_actor_last_status = Some("wasm-host:message_utf8_error".to_string());
                warn!(error = %err, "WASM host mailbox received non-UTF-8 payload");
                return;
            }
        };
        let command = match parse_plugin_host_command(&body) {
            Ok(command) => command,
            Err(err) => {
                self.plugin_actor_failures += 1;
                self.plugin_actor_last_status = Some("wasm-host:message_parse_error".to_string());
                warn!(body = %body, error = %err, "WASM host mailbox rejected message");
                return;
            }
        };
        self.apply_plugin_workspace_command(command, "wasm-host:message");
    }

    fn drain_plugin_host_mailbox(&mut self) {
        let Some(plugin_runtime) = self.plugin_runtime.as_mut() else {
            return;
        };
        for message in plugin_runtime.drain_host_messages() {
            self.apply_plugin_host_message(&message);
        }
    }

    fn apply_plugin_host_message(&mut self, message: &PluginHostMessage) {
        let command = match parse_plugin_host_command(&message.body) {
            Ok(command) => command,
            Err(err) => {
                self.plugin_actor_failures += 1;
                self.plugin_actor_last_status = Some("host:message_parse_error".to_string());
                warn!(
                    sender = %message.sender,
                    body = %message.body,
                    error = %err,
                    "plugin host mailbox rejected message"
                );
                return;
            }
        };
        self.apply_plugin_workspace_command(command, &format!("host:message:{}", message.sender));
    }

    fn apply_plugin_workspace_command(&mut self, command: WorkspaceCommand, status_context: &str) {
        if command.apply(self) {
            self.plugin_actor_last_status = Some(format!("{status_context}:applied"));
        } else {
            self.plugin_actor_failures += 1;
            self.plugin_actor_last_status = Some(format!("{status_context}:rejected"));
        }
    }

    fn handle_plugin_host_event(&mut self, event: PluginHostEvent) {
        match event {
            PluginHostEvent::DrainWasmHostMailbox => self.drain_wasm_host_mailbox(),
            PluginHostEvent::AppendPublicationFrame {
                actor_id,
                slot,
                frame,
            } => {
                let result = self
                    .plugin_runtime
                    .as_mut()
                    .context("plugin runtime is not running")
                    .and_then(|runtime| runtime.append_frame(&actor_id, &slot, frame));
                match result {
                    Ok(()) => {
                        self.plugin_actor_last_status =
                            Some(format!("{actor_id}:{slot}:frame_appended"));
                    }
                    Err(err) => {
                        self.plugin_actor_failures += 1;
                        self.plugin_actor_last_status =
                            Some(format!("{actor_id}:{slot}:frame_error"));
                        warn!(
                            plugin_actor = %actor_id,
                            publication_slot = %slot,
                            error = %err,
                            "plugin actor failed to append publication frame"
                        );
                    }
                }
            }
        }
    }

    pub const fn plugin_actor_message_count(&self) -> u64 {
        self.plugin_actor_messages
    }

    pub const fn plugin_actor_failure_count(&self) -> u64 {
        self.plugin_actor_failures
    }

    pub fn plugin_actor_last_status(&self) -> Option<&str> {
        self.plugin_actor_last_status.as_deref()
    }

    pub(crate) fn backend_actor_progress_snapshot(&self) -> BTreeMap<String, u64> {
        self.backend_actor_progress
            .snapshot(|actor_name, generation| {
                self.wasm_io_actors
                    .get(actor_name)
                    .is_some_and(|actor| actor.generation() == generation)
            })
    }

    fn record_backend_actor_progress(&mut self, actor_name: &str) {
        let Some(generation) = self
            .wasm_io_actors
            .get(actor_name)
            .map(brood_wasm::HostedWasmActor::generation)
        else {
            return;
        };
        self.backend_actor_progress.record(actor_name, generation);
    }

    fn drain_hosted_actor_events(&mut self) {
        enum InitRequest {
            StartDaemon(String, String, Vec<String>),
            StopDaemon(String),
            StartActor(SurfaceSessionId, String, String, Vec<String>),
            StopActor(SurfaceSessionId, String),
            StartApp(SurfaceSessionId, String, Vec<String>),
        }

        let _profile_span = brood_profiler::profile_span!("actor", "backend_event_drain");

        self.backend_actor_progress
            .retain_current(|actor_name, generation| {
                self.wasm_io_actors
                    .get(actor_name)
                    .is_some_and(|actor| actor.generation() == generation)
            });
        let mut events = Vec::with_capacity(BACKEND_ACTOR_EVENTS_PER_TURN);
        let mut init_requests = Vec::new();
        let mut local_messages = Vec::new();
        let mut presentation_messages = Vec::new();
        let mut host_messages = Vec::new();
        let actor_names = self.wasm_io_actors.keys().cloned().collect::<Vec<_>>();
        let actor_count = actor_names.len();
        let plan = bounded_round_robin_plan(
            actor_count,
            self.backend_actor_drain_cursor,
            BACKEND_ACTOR_EVENTS_PER_TURN,
            BACKEND_ACTOR_EVENTS_PER_ACTOR,
        );
        for (index, actor_budget) in &plan.actors {
            let Some(actor_name) = actor_names.get(*index) else {
                continue;
            };
            let Some(actor) = self.wasm_io_actors.get_mut(actor_name) else {
                continue;
            };
            match actor {
                brood_wasm::HostedWasmActor::Component(actor) => {
                    let mut actor_events = 0;
                    while actor_events < *actor_budget
                        && let Some(event) = actor.try_recv_event()
                    {
                        events.push((actor_name.clone(), Ok(event)));
                        actor_events += 1;
                    }
                }
                brood_wasm::HostedWasmActor::Preview1(actor) => {
                    if let Some(message) = actor.try_take_failure() {
                        events.push((actor_name.clone(), Err(message)));
                    }
                }
            }
        }

        self.backend_actor_drain_cursor = plan.next_cursor;

        // An actor omitted from this turn's bounded round-robin plan is not
        // necessarily ready. Counting every unvisited actor as backlog makes
        // this source self-wake forever as soon as the actor population exceeds
        // the eight actors covered by one 64-event turn. Inspect the bounded
        // observer queues after draining instead, so a wake represents work
        // which is actually queued.
        let observed_backlog =
            observed_actor_event_backlog(self.wasm_io_actors.values().map(|actor| match actor {
                brood_wasm::HostedWasmActor::Component(actor) => actor.pending_event_count(),
                brood_wasm::HostedWasmActor::Preview1(_) => 0,
            }));
        self.stats
            .backend_actor_events
            .record_batch(events.len(), observed_backlog);
        if observed_backlog > 0 {
            // A bounded drain must never require unrelated Wayland or input
            // traffic to resume actor progress.
            self.loop_signal.wakeup();
        }

        for (actor_name, event) in events {
            let event = match event {
                Ok(event) => event,
                Err(message) => {
                    self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
                    self.plugin_actor_last_status = Some(format!("{actor_name}:dispatcher_failed"));
                    warn!(actor = %actor_name, error = %message, "Preview 1 actor dispatcher failed");
                    if let Some(id) = self.daemon_child_owner(&actor_name) {
                        self.send_daemon_child_event(brood_wasm::ComponentChildEvent::Failed {
                            id,
                            message: message.clone(),
                        });
                    }
                    if let Some((session, child_id)) = self.realm_init_child_owner(&actor_name) {
                        self.send_realm_init_child_event(
                            session,
                            brood_wasm::ComponentChildEvent::Failed {
                                id: child_id,
                                message,
                            },
                        );
                    }
                    continue;
                }
            };
            if matches!(
                event,
                brood_wasm::ComponentActorEvent::DispatcherReturned
                    | brood_wasm::ComponentActorEvent::PresentationTurnReturned { .. }
            ) {
                self.record_backend_actor_progress(&actor_name);
            }
            match event {
                brood_wasm::ComponentActorEvent::DispatcherFailed { message } => {
                    self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
                    self.plugin_actor_last_status = Some(format!("{actor_name}:dispatcher_failed"));
                    warn!(actor = %actor_name, error = %message, "component actor dispatcher failed");
                    if actor_name == BACKEND_INIT_ACTOR_ID {
                        self.fail_pending_managed_worktrees(&format!(
                            "worktree backend stopped unexpectedly: {message}"
                        ));
                    }
                    if let Some(id) = self.daemon_child_owner(&actor_name) {
                        self.send_daemon_child_event(brood_wasm::ComponentChildEvent::Failed {
                            id,
                            message: message.clone(),
                        });
                    }
                    if let Some((session, child_id)) = self.realm_init_child_owner(&actor_name) {
                        self.send_realm_init_child_event(
                            session,
                            brood_wasm::ComponentChildEvent::Failed {
                                id: child_id,
                                message,
                            },
                        );
                    }
                }
                brood_wasm::ComponentActorEvent::WorkerFinished { worker, .. } => {
                    self.plugin_actor_last_status =
                        Some(format!("{actor_name}:worker_{}_finished", worker.raw()));
                    info!(
                        actor = %actor_name,
                        worker = worker.raw(),
                        "component actor worker finished"
                    );
                }
                brood_wasm::ComponentActorEvent::WorkerFailed { worker, message } => {
                    self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
                    self.plugin_actor_last_status =
                        Some(format!("{actor_name}:worker_{}_failed", worker.raw()));
                    warn!(actor = %actor_name, worker = worker.raw(), error = %message, "component actor worker failed");
                }
                brood_wasm::ComponentActorEvent::WorkerCancelled { worker } => {
                    self.plugin_actor_last_status =
                        Some(format!("{actor_name}:worker_{}_cancelled", worker.raw()));
                    info!(
                        actor = %actor_name,
                        worker = worker.raw(),
                        "component actor worker cancelled"
                    );
                }
                brood_wasm::ComponentActorEvent::Stopped => {
                    self.plugin_actor_last_status = Some(format!("{actor_name}:stopped"));
                    info!(actor = %actor_name, "component actor stopped");
                    if actor_name == BACKEND_INIT_ACTOR_ID {
                        self.fail_pending_managed_worktrees(
                            "worktree backend stopped before creation completed",
                        );
                    }
                    if let Some(id) = self.daemon_child_owner(&actor_name) {
                        self.send_daemon_child_event(brood_wasm::ComponentChildEvent::Stopped {
                            id,
                        });
                    }
                    if let Some((session, child_id)) = self.realm_init_child_owner(&actor_name) {
                        self.send_realm_init_child_event(
                            session,
                            brood_wasm::ComponentChildEvent::Stopped { id: child_id },
                        );
                    }
                }
                brood_wasm::ComponentActorEvent::ActorStartRequested {
                    id,
                    path,
                    capabilities,
                    presentation_scope: _,
                } => {
                    if actor_name == BACKEND_INIT_ACTOR_ID {
                        init_requests.push(InitRequest::StartDaemon(id, path, capabilities));
                    } else if let Some(session) = self.realm_init_root_session(&actor_name) {
                        init_requests.push(InitRequest::StartActor(
                            session,
                            id,
                            path,
                            capabilities,
                        ));
                    }
                }
                brood_wasm::ComponentActorEvent::ActorStopRequested { id } => {
                    if actor_name == BACKEND_INIT_ACTOR_ID {
                        init_requests.push(InitRequest::StopDaemon(id));
                    } else if let Some(session) = self.realm_init_root_session(&actor_name) {
                        init_requests.push(InitRequest::StopActor(session, id));
                    }
                }
                brood_wasm::ComponentActorEvent::AppStartRequested { id, argv } => {
                    if let Some(session) = self.realm_init_root_session(&actor_name) {
                        init_requests.push(InitRequest::StartApp(session, id, argv));
                    }
                }
                brood_wasm::ComponentActorEvent::LocalMessageRequested { actor, body } => {
                    local_messages.push((actor_name, actor, body));
                }
                brood_wasm::ComponentActorEvent::PresentationMessageRequested { actor, body } => {
                    presentation_messages.push((actor_name, actor, body));
                }
                brood_wasm::ComponentActorEvent::HostMessageRequested { payload } => {
                    host_messages.push((actor_name, payload));
                }
                _ => {}
            }
        }
        for (actor_name, payload) in host_messages {
            let policy_scope = if actor_name == BACKEND_INIT_ACTOR_ID {
                Some((
                    brood_cli::package_runtime::contract::Realm::Daemon,
                    "daemon".to_string(),
                ))
            } else {
                self.realm_init_root_session(&actor_name).map(|session| {
                    (
                        brood_cli::package_runtime::contract::Realm::SurfaceSession,
                        session.raw().to_string(),
                    )
                })
            };
            if let Some((realm, scope)) = policy_scope
                && let Some(actor) = self.wasm_io_actors.get(&actor_name)
                && self.package_host.policy_report(
                    realm,
                    &scope,
                    &actor_name,
                    actor.generation(),
                    &payload,
                )
            {
                continue;
            }
            if let Some(actor) = self.wasm_io_actors.get(&actor_name)
                && self.package_host.service_request(
                    &actor_name,
                    actor.generation(),
                    &payload,
                    || self.package_resource_samples(),
                )
            {
                continue;
            }
            if actor_name == BACKEND_INIT_ACTOR_ID {
                self.handle_backend_actor_host_message(&payload);
            } else {
                warn!(actor = %actor_name, "ignored server component host message outside the backend realm");
            }
        }
        for (sender_actor, recipient, body) in local_messages {
            let owner = self
                .realm_init_root_session(&sender_actor)
                .map(|session| (session, "init".to_string()))
                .or_else(|| self.realm_init_child_owner(&sender_actor));
            let Some((session, sender)) = owner else {
                warn!(actor = %sender_actor, "rejected local message from actor outside a session realm");
                continue;
            };
            if let Err(error) =
                self.send_local_realm_actor_message(session, &recipient, &sender, body)
            {
                warn!(
                    actor = %sender_actor,
                    recipient = %recipient,
                    error = %error,
                    "rejected local session actor message"
                );
            }
        }
        for (sender_actor, recipient, body) in presentation_messages {
            let owner = if sender_actor == BACKEND_INIT_ACTOR_ID {
                Some((BackendActorRealm::Daemon, "init".to_string()))
            } else {
                self.realm_init_root_session(&sender_actor)
                    .map(|session| {
                        (
                            BackendActorRealm::Session(session.raw()),
                            "init".to_string(),
                        )
                    })
                    .or_else(|| {
                        self.realm_init_child_owner(&sender_actor)
                            .map(|(session, sender)| {
                                (BackendActorRealm::Session(session.raw()), sender)
                            })
                    })
            };
            let Some((realm, sender)) = owner else {
                warn!(actor = %sender_actor, "rejected presentation message from actor outside a backend realm");
                continue;
            };
            if let Err(error) =
                self.send_presentation_actor_message(realm, &sender, &recipient, &body)
            {
                warn!(
                    actor = %sender_actor,
                    recipient = %recipient,
                    error = %error,
                    "rejected session-to-presentation actor message"
                );
            }
        }
        for request in init_requests {
            match request {
                InitRequest::StartDaemon(id, path, capabilities) => {
                    self.handle_daemon_child_start(id, &path, &capabilities);
                }
                InitRequest::StopDaemon(id) => self.handle_daemon_child_stop(id),
                InitRequest::StartActor(session, id, path, capabilities) => {
                    self.handle_realm_init_start(session, id, &path, &capabilities);
                }
                InitRequest::StopActor(session, id) => {
                    self.handle_realm_init_stop(session, id);
                }
                InitRequest::StartApp(session, id, argv) => {
                    self.handle_realm_init_app_start(session, id, &argv);
                }
            }
        }
    }

    pub fn register_surface(&mut self, surface: &WlSurface) {
        if !self.known_surfaces.contains(surface) {
            self.known_surfaces.push(surface.clone());
        }
        self.update_fractional_scale_for_surface(surface);
    }

    pub fn unregister_surface(&mut self, surface: &WlSurface) {
        self.known_surfaces.retain(|known| known != surface);
        if let Some(pane) = self.pane_for_toplevel_surface(surface) {
            if let Some(fast_path) = self.dmabuf.fast_path.as_mut() {
                for correlation in fast_path.managed_resizes.remove_pane(pane) {
                    if managed_resize_needs_cleanup_rejection(&correlation.state) {
                        emit_managed_resize_status(
                            fast_path,
                            brood_protocol::ManagedResizeStatus {
                                pane,
                                resize_epoch: correlation.epoch,
                                result: brood_protocol::ManagedResizeStatusResult::Rejected {
                                    reason: brood_protocol::ManagedResizeRejectionReason::PaneUnavailable,
                                },
                            },
                        );
                    } else {
                        info!(
                            pane,
                            epoch = correlation.epoch,
                            "removed committed managed resize correlation without late rejection"
                        );
                    }
                }
                crate::frame_export::cleanup_provisional_tagged_resources(fast_path);
            }
            self.presentation_client_requested_sizes.remove(&pane);
        }
    }

    pub fn refresh_fractional_scales(&mut self) {
        let scale = self.current_fractional_scale();
        self.known_surfaces.retain(IsAlive::alive);
        for surface in &self.known_surfaces {
            set_fractional_scale_for_surface(surface, scale);
        }
    }

    pub fn update_fractional_scale_for_surface(&self, surface: &WlSurface) {
        set_fractional_scale_for_surface(surface, self.current_fractional_scale());
    }

    fn current_fractional_scale(&self) -> f64 {
        self.session_output.current_scale().fractional_scale()
    }

    fn init_session_output(display_handle: &DisplayHandle) -> Output {
        let output = Output::new(
            "brood-session".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Brood".into(),
                model: "Session".into(),
            },
        );
        let mode = OutputMode {
            size: SESSION_OUTPUT_SIZE.into(),
            refresh: SESSION_OUTPUT_REFRESH_MILLIHZ,
        };
        output.create_global::<Self>(display_handle);
        output.change_current_state(
            Some(mode),
            Some(Transform::Flipped180),
            Some(OutputScale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        output
    }

    fn init_wayland_listener(
        display: Display<Self>,
        event_loop: &mut EventLoop<'static, Self>,
    ) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto().expect("wayland socket");
        let socket_name = listening_socket.socket_name().to_os_string();
        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, (), state| {
                let started = Instant::now();
                let driver_before_micros = state.stats.smithay_driver_total_micros;
                let result = state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()));
                if let Err(err) = result {
                    state.stats.wayland_client_insert_errors += 1;
                    warn!(?err, "insert Wayland client failed");
                    state
                        .stats
                        .record_smithay_callback(started.elapsed(), driver_before_micros);
                    return;
                }
                if let Err(err) = state.display_handle.flush_clients() {
                    warn!(?err, "flush clients after insert failed");
                }
                state
                    .stats
                    .record_smithay_callback(started.elapsed(), driver_before_micros);
            })
            .expect("insert wayland socket");

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    let started = Instant::now();
                    let driver_before_micros = state.stats.smithay_driver_total_micros;
                    dispatch_wayland_clients(display, state);
                    if let Err(err) = state.display_handle.flush_clients() {
                        warn!(?err, "flush clients after dispatch failed");
                    }
                    state
                        .stats
                        .record_smithay_callback(started.elapsed(), driver_before_micros);
                    Ok(PostAction::Continue)
                },
            )
            .expect("insert wayland display");

        socket_name
    }

    fn init_hosted_child_event_source(
        event_loop: &mut EventLoop<'static, Self>,
    ) -> channel::Sender<HostedChildEvent> {
        let (sender, receiver) = channel::channel();
        event_loop
            .handle()
            .insert_source(receiver, |event, (), state| {
                let started = Instant::now();
                let driver_before_micros = state.stats.smithay_driver_total_micros;
                let channel::Event::Msg(event) = event else {
                    return;
                };
                match event {
                    HostedChildEvent::Started(started) => {
                        state.record_hosted_child_started(started);
                    }
                    HostedChildEvent::SpawnFailed(failure) => {
                        state.record_hosted_child_spawn_failure(&failure);
                    }
                    HostedChildEvent::Exited(exit) => state.record_hosted_child_exit(exit),
                }
                state.refresh_window_state_from_lifecycle_event();
                state
                    .stats
                    .record_smithay_callback(started.elapsed(), driver_before_micros);
            })
            .expect("insert hosted child event source");
        sender
    }

    fn init_host_clipboard_bridge(
        event_loop: &mut EventLoop<'static, Self>,
    ) -> Option<HostClipboardBridge> {
        let (sender, receiver) = channel::channel();
        event_loop
            .handle()
            .insert_source(receiver, |event, (), state| {
                let started = Instant::now();
                let driver_before_micros = state.stats.smithay_driver_total_micros;
                let channel::Event::Msg(event) = event else {
                    return;
                };
                state.handle_host_clipboard_event(event);
                state
                    .stats
                    .record_smithay_callback(started.elapsed(), driver_before_micros);
            })
            .expect("insert host clipboard event source");
        HostClipboardBridge::start(sender)
    }

    fn init_plugin_host_event_source(
        event_loop: &mut EventLoop<'static, Self>,
    ) -> channel::Sender<PluginHostEvent> {
        let (sender, receiver) = channel::channel();
        event_loop
            .handle()
            .insert_source(receiver, |event, (), state| {
                let started = Instant::now();
                let driver_before_micros = state.stats.smithay_driver_total_micros;
                let channel::Event::Msg(event) = event else {
                    return;
                };
                state.handle_plugin_host_event(event);
                state
                    .stats
                    .record_smithay_callback(started.elapsed(), driver_before_micros);
            })
            .expect("insert plugin host event source");
        sender
    }

    pub fn spawn_hosted_app(&mut self, command: &OsString) -> Result<HostedChildToken> {
        self.spawn_hosted_app_in_session(command, self.sessions.active())
    }

    pub fn spawn_hosted_app_in_session(
        &mut self,
        command: &OsString,
        session: SurfaceSessionId,
    ) -> Result<HostedChildToken> {
        ensure!(
            self.sessions.contains(session),
            "unknown surface session {}",
            session.raw()
        );
        let pane_id = self.allocate_pane_id();
        let mut command_builder = Command::new(command);
        command_builder.env("WAYLAND_DISPLAY", &self.socket_name);
        command_builder.env(BROOD_PANE_ID_ENV, pane_id.to_string());
        command_builder.env(BROOD_SURFACE_SESSION_ID_ENV, session.raw().to_string());
        self.apply_session_working_directory(&mut command_builder, session);
        self.apply_control_environment(&mut command_builder, session);
        self.launch_hosted_child(
            command.to_string_lossy().into_owned(),
            command_builder,
            pane_id,
            session,
            format!(
                "spawn hosted Wayland app {:?} with WAYLAND_DISPLAY={}",
                command,
                self.socket_name.to_string_lossy()
            ),
        )
    }

    pub fn spawn_hosted_app_argv(&mut self, argv: &[String]) -> Result<HostedChildToken> {
        self.spawn_hosted_app_argv_in_session(argv, self.sessions.active())
    }

    pub fn spawn_hosted_app_argv_in_session(
        &mut self,
        argv: &[String],
        session: SurfaceSessionId,
    ) -> Result<HostedChildToken> {
        let Some((program, args)) = argv.split_first() else {
            anyhow::bail!("spawn requires a command");
        };
        ensure!(
            self.sessions.contains(session),
            "unknown surface session {}",
            session.raw()
        );

        let pane_id = self.allocate_pane_id();
        let mut command_builder = Command::new(program);
        command_builder
            .args(args)
            .env("WAYLAND_DISPLAY", &self.socket_name);
        command_builder.env(BROOD_PANE_ID_ENV, pane_id.to_string());
        command_builder.env(BROOD_SURFACE_SESSION_ID_ENV, session.raw().to_string());
        self.apply_session_working_directory(&mut command_builder, session);
        self.apply_control_environment(&mut command_builder, session);
        self.launch_hosted_child(
            argv.join(" "),
            command_builder,
            pane_id,
            session,
            format!(
                "spawn hosted Wayland app {:?} with WAYLAND_DISPLAY={}",
                argv,
                self.socket_name.to_string_lossy()
            ),
        )
    }

    fn launch_hosted_child(
        &mut self,
        command_label: String,
        mut command: Command,
        pane_id: u64,
        session: SurfaceSessionId,
        spawn_context: String,
    ) -> Result<HostedChildToken> {
        self.pending_child_panes.push(PendingChildPane {
            pid: None,
            pane_id,
            session,
        });
        if let Some(runtime) = self.async_runtime.clone() {
            self.spawn_hosted_child_launch_task(&runtime, command_label, command, pane_id, session);
            return Ok(HostedChildToken(pane_id));
        }

        match command.spawn() {
            Ok(child) => {
                let pid = child.id();
                self.mark_pending_child_started(pane_id, pid);
                info!(
                    command = %command_label,
                    pane_id,
                    surface_session = session.raw(),
                    wayland_display = %self.socket_name.to_string_lossy(),
                    "spawned hosted Wayland app"
                );
                self.track_hosted_child(command_label, pid, Some(child), Some(pane_id));
                Ok(HostedChildToken(pane_id))
            }
            Err(err) => {
                self.pending_child_panes
                    .retain(|pending| pending.pane_id != pane_id);
                Err(err).with_context(|| spawn_context)
            }
        }
    }

    fn track_hosted_child(
        &mut self,
        command: String,
        pid: u32,
        child: Option<Child>,
        pane_id: Option<u64>,
    ) {
        self.hosted_children.push(HostedChild {
            command,
            child,
            pid,
            pane_id,
        });
    }

    fn mark_pending_child_started(&mut self, pane_id: u64, pid: u32) {
        if let Some(pending) = self
            .pending_child_panes
            .iter_mut()
            .rev()
            .find(|pending| pending.pane_id == pane_id)
        {
            pending.pid = Some(pid);
        }
    }

    fn spawn_hosted_child_launch_task(
        &self,
        runtime: &tokio::runtime::Handle,
        command: String,
        mut child_command: Command,
        pane_id: u64,
        session: SurfaceSessionId,
    ) {
        let sender = self.hosted_child_event_sender.clone();
        let task = runtime.spawn_blocking(move || match child_command.spawn() {
            Ok(mut child) => {
                let pid = child.id();
                sender
                    .send(HostedChildEvent::Started(HostedChildStarted {
                        pid,
                        command: command.clone(),
                        pane_id,
                        session,
                    }))
                    .ok();
                let result = child.wait();
                sender
                    .send(HostedChildEvent::Exited(HostedChildExit {
                        pid,
                        command,
                        pane_id: Some(pane_id),
                        result,
                    }))
                    .ok();
            }
            Err(err) => {
                sender
                    .send(HostedChildEvent::SpawnFailed(HostedChildSpawnFailure {
                        command,
                        pane_id,
                        error: err.to_string(),
                    }))
                    .ok();
            }
        });
        drop(task);
    }

    fn record_hosted_child_started(&mut self, started: HostedChildStarted) {
        self.mark_pending_child_started(started.pane_id, started.pid);
        info!(
            command = %started.command,
            pid = started.pid,
            pane_id = started.pane_id,
            surface_session = started.session.raw(),
            wayland_display = %self.socket_name.to_string_lossy(),
            "spawned hosted Wayland app"
        );
        self.track_hosted_child(started.command, started.pid, None, Some(started.pane_id));
    }

    fn record_hosted_child_spawn_failure(&mut self, failure: &HostedChildSpawnFailure) {
        self.pending_child_panes
            .retain(|pending| pending.pane_id != failure.pane_id);
        self.hosted_child_failures += 1;
        self.hosted_child_last_status = Some(format!("{}:spawn_error", failure.command));
        remember_hosted_child_completion(
            &mut self.hosted_child_completions,
            Some(failure.pane_id),
            false,
            format!("spawn_error:{}", failure.error),
        );
        warn!(
            error = %failure.error,
            command = %failure.command,
            pane_id = failure.pane_id,
            "failed to spawn hosted Wayland app"
        );
    }

    pub fn refresh_hosted_children(&mut self) {
        let mut active = Vec::with_capacity(self.hosted_children.len());
        for mut hosted in self.hosted_children.drain(..) {
            let Some(child) = hosted.child.as_mut() else {
                active.push(hosted);
                continue;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    let success = status.success();
                    let detail = hosted_child_exit_detail(status);
                    record_hosted_child_exit_status(
                        &mut self.hosted_child_exits,
                        &mut self.hosted_child_failures,
                        &mut self.hosted_child_last_status,
                        &hosted.command,
                        hosted.pane_id,
                        status,
                    );
                    remember_hosted_child_completion(
                        &mut self.hosted_child_completions,
                        hosted.pane_id,
                        success,
                        detail,
                    );
                }
                Ok(None) => active.push(hosted),
                Err(err) => {
                    let detail = format!("poll_error:{err}");
                    record_hosted_child_exit_error(
                        &mut self.hosted_child_exits,
                        &mut self.hosted_child_failures,
                        &mut self.hosted_child_last_status,
                        &hosted.command,
                        hosted.pane_id,
                        &err,
                        "poll_error",
                    );
                    remember_hosted_child_completion(
                        &mut self.hosted_child_completions,
                        hosted.pane_id,
                        false,
                        detail,
                    );
                }
            }
        }
        self.hosted_children = active;
    }

    fn record_hosted_child_exit(&mut self, exit: HostedChildExit) {
        let mut matched = false;
        self.hosted_children.retain(|hosted| {
            let keep = hosted.pid != exit.pid;
            matched |= !keep;
            keep
        });
        if !matched {
            warn!(
                pid = exit.pid,
                command = %exit.command,
                pane_id = ?exit.pane_id,
                "hosted Wayland app exit arrived after child record was gone"
            );
        }
        self.pending_child_panes
            .retain(|pending| Some(pending.pane_id) != exit.pane_id);
        match exit.result {
            Ok(status) => {
                let success = status.success();
                let detail = hosted_child_exit_detail(status);
                record_hosted_child_exit_status(
                    &mut self.hosted_child_exits,
                    &mut self.hosted_child_failures,
                    &mut self.hosted_child_last_status,
                    &exit.command,
                    exit.pane_id,
                    status,
                );
                remember_hosted_child_completion(
                    &mut self.hosted_child_completions,
                    exit.pane_id,
                    success,
                    detail,
                );
            }
            Err(err) => {
                let detail = format!("wait_error:{err}");
                record_hosted_child_exit_error(
                    &mut self.hosted_child_exits,
                    &mut self.hosted_child_failures,
                    &mut self.hosted_child_last_status,
                    &exit.command,
                    exit.pane_id,
                    &err,
                    "wait_error",
                );
                remember_hosted_child_completion(
                    &mut self.hosted_child_completions,
                    exit.pane_id,
                    false,
                    detail,
                );
            }
        }
    }

    pub(crate) fn hosted_child_status(&self, token: HostedChildToken) -> HostedChildStatus {
        if self
            .hosted_children
            .iter()
            .any(|child| child.pane_id == Some(token.raw()))
        {
            return HostedChildStatus::Running;
        }
        if let Some((_, success, detail)) = self
            .hosted_child_completions
            .iter()
            .rev()
            .find(|(completed, _, _)| *completed == token)
        {
            return HostedChildStatus::Exited {
                success: *success,
                detail: detail.clone(),
            };
        }
        if self
            .pending_child_panes
            .iter()
            .any(|pending| pending.pane_id == token.raw())
        {
            return HostedChildStatus::Launching;
        }
        HostedChildStatus::Unknown
    }

    pub(crate) fn hosted_child_visible(&self, token: HostedChildToken) -> bool {
        self.surface_for_pane(token.raw()).is_some()
    }

    pub const fn hosted_child_count(&self) -> usize {
        self.hosted_children.len()
    }

    pub const fn hosted_child_exits(&self) -> u64 {
        self.hosted_child_exits
    }

    pub const fn hosted_child_failures(&self) -> u64 {
        self.hosted_child_failures
    }

    pub fn hosted_child_last_status(&self) -> &str {
        self.hosted_child_last_status.as_deref().unwrap_or("none")
    }

    pub fn spawn_browser_pane(
        &mut self,
        options: BrowserLaunchOptions,
    ) -> Result<&BrowserInstanceSummary> {
        self.browsers.spawn(options, &self.socket_name)
    }

    fn apply_control_environment(&mut self, child: &mut Command, session: SurfaceSessionId) {
        child.envs(self.session_process_environment(session));
    }

    fn session_process_environment(
        &mut self,
        session: SurfaceSessionId,
    ) -> BTreeMap<OsString, OsString> {
        let mut environment = BTreeMap::from([
            (OsString::from("WAYLAND_DISPLAY"), self.socket_name.clone()),
            (
                OsString::from(BROOD_SURFACE_SESSION_ID_ENV),
                OsString::from(session.raw().to_string()),
            ),
        ]);
        if let Some(session) = self.brood_session.as_deref() {
            environment.insert(OsString::from(BROOD_SESSION_ENV), OsString::from(session));
        }
        if let Some(destination) = self.brood_dbus_destination.as_deref() {
            environment.insert(
                OsString::from(BROODD_DBUS_DESTINATION_ENV),
                OsString::from(destination),
            );
        }
        if let Some(address) = self.brood_dbus_address.clone() {
            environment.insert(
                OsString::from(BROODD_DBUS_ADDRESS_ENV),
                OsString::from(&address),
            );
            match self.ensure_session_dbus_bus(session) {
                Ok(session_bus_address) => {
                    environment.insert(
                        OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                        OsString::from(session_bus_address),
                    );
                }
                Err(err) => {
                    warn!(
                        ?err,
                        session = session.raw(),
                        "failed to start session DBus bus; managed client inherits Brood control bus"
                    );
                    environment.insert(
                        OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                        OsString::from(address),
                    );
                }
            }
        } else {
            warn!(
                "managed client inherits host DBUS_SESSION_BUS_ADDRESS; DBus policy modes off/proxy/private are not enforced yet"
            );
        }
        environment
    }

    fn apply_session_working_directory(&self, child: &mut Command, session: SurfaceSessionId) {
        if let Some(directory) = self.sessions.working_directory(session) {
            child.current_dir(directory);
        }
    }

    fn ensure_session_dbus_bus(&mut self, session: SurfaceSessionId) -> Result<String> {
        let realm = SessionBusRealm::Surface(session.raw());
        if let Some(bus) = self
            .sessions
            .buses()
            .iter()
            .find(|(bus_realm, _)| *bus_realm == realm)
        {
            return Ok(bus.1.address().to_string());
        }

        let handle = if let Some(runtime) = self.async_runtime.as_ref() {
            SessionBusHandle::start_on_runtime(realm, self.host_dbus_address.clone(), runtime)
        } else {
            SessionBusHandle::start(realm, self.host_dbus_address.clone())
        }
        .context("start in-process session DBus bus")?;
        let address = handle.address().to_string();
        self.sessions.add_bus(realm, handle);
        info!(
            session = session.raw(),
            dbus_address = %address,
            "started in-process session-local DBus bus"
        );
        Ok(address)
    }

    pub fn configure_external_presentation_output(
        &mut self,
        width: i32,
        height: i32,
        refresh_millihz: i32,
        scale_120: u32,
    ) {
        self.configure_presentation_output_inner(width, height, refresh_millihz, scale_120, false);
    }

    fn configure_presentation_output_inner(
        &mut self,
        width: i32,
        height: i32,
        refresh_millihz: i32,
        scale_120: u32,
        configure_managed_windows: bool,
    ) {
        let size = Size::from((width.max(1), height.max(1)));
        let refresh = if refresh_millihz > 0 {
            refresh_millihz
        } else {
            self.session_output
                .current_mode()
                .map_or(SESSION_OUTPUT_REFRESH_MILLIHZ, |mode| mode.refresh)
        };
        let mode = OutputMode { size, refresh };
        let scale = OutputScale::Fractional(f64::from(scale_120.max(1)) / 120.0);
        self.session_output
            .change_current_state(Some(mode), None, Some(scale), None);
        self.session_output.set_preferred(mode);
        self.refresh_fractional_scales();
        if configure_managed_windows {
            self.reconfigure_focus_stack();
        }
        crate::frame_export::request_redraw(self);
        if let Err(error) = self.display_handle.flush_clients() {
            warn!(?error, "flush after presentation client resize failed");
        }
    }

    pub fn allocate_toplevel_location(&mut self) -> Point<i32, Logical> {
        let slot = self.next_window_slot;
        self.next_window_slot += 1;
        focus_stack_location(slot)
    }

    pub fn record_dmabuf_blocker_failure(&mut self, error: &str) {
        self.dmabuf.stats.blocker_install_failures += 1;
        self.dmabuf.stats.last_blocker_error =
            Some(crate::frame_export::sanitize_status_value(error));
    }

    pub fn service_test_harness(&mut self) {
        let Some(mut harness) = self.test_harness.take_runtime() else {
            return;
        };

        for request in harness.take_pending_requests() {
            service_request(self, request);
        }

        if !self.test_harness.is_complete() {
            match harness.try_take_completion() {
                Ok(Some(completion)) => self.record_test_harness_completion(completion),
                Ok(None) => {}
                Err(err) => {
                    warn!(?err, "test harness lifecycle transition rejected");
                    self.record_test_harness_completion(TestHarnessCompletion {
                        kind: TestHarnessOutcomeKind::Failed,
                        exit_code: 1,
                        detail: err.to_string(),
                    });
                }
            }
        }

        if let Err(err) = self.test_harness.restore_runtime(harness) {
            warn!(err, "test harness runtime ownership transition rejected");
        }
    }

    fn record_test_harness_completion(&mut self, completion: TestHarnessCompletion) {
        match completion.kind {
            TestHarnessOutcomeKind::Passed => {
                info!(detail = %completion.detail, "test harness passed");
                self.loop_signal.stop();
            }
            TestHarnessOutcomeKind::Failed => {
                warn!(
                    exit_code = completion.exit_code,
                    detail = %completion.detail,
                    "test harness failed"
                );
                self.loop_signal.stop();
            }
            TestHarnessOutcomeKind::Paused => {
                warn!(
                    exit_code = completion.exit_code,
                    detail = %completion.detail,
                    "test harness paused for interactive inspection"
                );
            }
        }
        if let Err(err) = self.test_harness.record_completion(completion) {
            warn!(err, "test harness completion transition rejected");
        }
    }

    pub(crate) fn shutdown_test_harness(&mut self) -> Result<()> {
        let Some(mut harness) = self.test_harness.take_runtime() else {
            return Ok(());
        };
        if !self.test_harness.is_complete()
            && let Some(completion) = harness.try_take_completion()?
        {
            self.record_test_harness_completion(completion);
        }
        let cleanup = harness.cleanup();
        if !self.test_harness.is_complete()
            && let Some(completion) = harness.try_take_completion()?
        {
            self.record_test_harness_completion(completion);
        }
        let cleanup_succeeded = cleanup.is_success();
        self.test_harness
            .record_cleanup(cleanup.clone())
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            cleanup_succeeded,
            "test harness cleanup failed: {cleanup:?}"
        );
        self.test_harness
            .restore_runtime(harness)
            .map_err(anyhow::Error::msg)
    }

    pub fn signal_test_harness(&self, name: String) -> Result<()> {
        let Some(harness) = self.test_harness.runtime() else {
            anyhow::bail!("no running test harness");
        };
        harness.send_signal(name)
    }

    pub(crate) fn install_test_harness(&mut self, harness: TestHarnessRuntime) -> Result<()> {
        self.test_harness
            .install(harness)
            .map_err(anyhow::Error::msg)
    }

    pub(crate) fn configure_nested_test_target(
        &mut self,
        destination: Option<String>,
        address: Option<String>,
    ) -> Result<()> {
        self.test_harness
            .configure_nested_target(destination, address)
            .map_err(anyhow::Error::msg)
    }

    pub(crate) fn nested_test_target(&self) -> Option<(&str, Option<&str>)> {
        self.test_harness.nested_target()
    }

    pub(crate) const fn test_harness_completion(&self) -> Option<&TestHarnessCompletion> {
        self.test_harness.completion()
    }

    pub(crate) const fn test_harness_cleanup(
        &self,
    ) -> Option<&brood_wasm::TestHarnessCleanupResult> {
        self.test_harness.cleanup()
    }

    pub fn activate_window_exclusively(&mut self, active_surface: Option<&WlSurface>) {
        self.activate_windows_without_geometry(active_surface);
    }

    fn activate_windows_without_geometry(&mut self, active_surface: Option<&WlSurface>) {
        self.space.elements().for_each(|window| {
            let Some(toplevel) = window.toplevel() else {
                return;
            };
            let is_active = active_surface == Some(toplevel.wl_surface());
            toplevel.with_pending_state(|state| {
                if is_active {
                    state.states.set(xdg_toplevel::State::Activated);
                } else {
                    state.states.unset(xdg_toplevel::State::Activated);
                }
            });
            window.set_activated(is_active);
            toplevel.send_pending_configure();
        });
    }

    pub fn window_toplevel_surface(window: &Window) -> Option<WlSurface> {
        window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone())
    }

    pub fn record_window_mapped(&mut self) {
        self.last_window_count = self.presentation_window_count();
        self.reconfigure_focus_stack();
    }

    pub fn assign_new_toplevel_surface(&mut self, surface: &WlSurface) -> Option<WlSurface> {
        let registered = self.register_toplevel_pane_and_session(surface);
        if let Err(err) = self.sessions.assign_surface(surface, registered.session) {
            warn!(?err, "refusing invalid toplevel session assignment");
            return None;
        }
        self.apply_registered_extension_grants_for_surface(surface);
        self.emit_wasm_actor_event(
            brood_wasm::WasmActorEventKind::PaneMapped,
            &format!(
                r#"{{"type":"pane-mapped","pane":{},"session":{}}}"#,
                registered.pane_id,
                registered.session.raw()
            ),
        );
        Some(surface.clone())
    }

    fn register_toplevel_pane_and_session(
        &mut self,
        surface: &WlSurface,
    ) -> RegisteredToplevelPane {
        if let Some(pane) = self.pane_for_toplevel_surface(surface) {
            return RegisteredToplevelPane {
                pane_id: pane,
                session: self
                    .session_for_toplevel_surface(surface)
                    .unwrap_or_else(|| self.default_surface_session()),
            };
        }
        let pending = self.claim_pending_child_pane(surface);
        let pane = pending
            .as_ref()
            .map_or_else(|| self.allocate_pane_id(), |pending| pending.pane_id);
        let session = pending
            .as_ref()
            .map(|pending| pending.session)
            .or_else(|| self.session_from_toplevel_process(surface))
            .unwrap_or_else(|| self.default_surface_session());
        self.window_pane_ids.push((surface.clone(), pane));
        RegisteredToplevelPane {
            pane_id: pane,
            session,
        }
    }

    fn apply_registered_extension_grants_for_surface(&mut self, surface: &WlSurface) {
        let window_ids = self
            .extension_registry
            .windows
            .iter()
            .filter(|window| toplevel_matches_identity(self, surface, &window.identity))
            .map(|window| window.id.clone())
            .collect::<Vec<_>>();
        if window_ids.is_empty() {
            return;
        }

        let grants = self
            .extension_registry
            .grants
            .iter()
            .filter(|grant| grant.active)
            .filter(|grant| {
                grant
                    .detail
                    .get("window_id")
                    .is_some_and(|id| window_ids.iter().any(|window_id| window_id == id))
            })
            .map(|grant| (grant.kind.clone(), grant.detail.clone()))
            .collect::<Vec<_>>();

        for (kind, detail) in grants {
            if kind == "placement" {
                if let Some(session) = detail
                    .get("surface_session")
                    .or_else(|| detail.get("session_id"))
                    .and_then(|value| self.resolve_surface_session(value))
                {
                    self.assign_toplevel_surface_session(surface, session);
                }
                if detail
                    .get("focus")
                    .is_some_and(|value| matches!(value.as_str(), "true" | "1" | "yes"))
                {
                    self.focus_toplevel_surface(Some(surface.clone()));
                }
            }
        }
    }

    const fn allocate_pane_id(&mut self) -> u64 {
        let pane = self.next_pane_id;
        self.next_pane_id += 1;
        pane
    }

    fn claim_pending_child_pane(&mut self, surface: &WlSurface) -> Option<PendingChildPane> {
        let surface_pid = toplevel_pid(self, surface)?;
        let index = self
            .pending_child_panes
            .iter()
            .position(|pending| {
                let Some(pending_pid) = pending.pid else {
                    return false;
                };
                let pending_pid = pending_pid.to_string();
                surface_pid == pending_pid
                    || process_is_ancestor(&surface_pid, &pending_pid)
                    || process_is_ancestor(&pending_pid, &surface_pid)
            })
            .or_else(|| {
                let surface_pid = surface_pid.parse::<u32>().ok()?;
                let pane_id = process_tree_env_var(surface_pid, BROOD_PANE_ID_ENV)?
                    .parse::<u64>()
                    .ok()?;
                self.pending_child_panes
                    .iter()
                    .position(|pending| pending.pane_id == pane_id)
            })?;
        Some(self.pending_child_panes.remove(index))
    }

    fn session_from_toplevel_process(&self, surface: &WlSurface) -> Option<SurfaceSessionId> {
        let pid = toplevel_pid(self, surface)?.parse::<u32>().ok()?;
        process_tree_env_var(pid, BROOD_SURFACE_SESSION_ID_ENV)
            .and_then(|value| value.parse::<u64>().ok())
            .map(SurfaceSessionId::from_raw)
            .filter(|session| self.sessions.contains(*session))
    }

    fn default_surface_session(&self) -> SurfaceSessionId {
        self.sessions
            .sessions()
            .first()
            .copied()
            .unwrap_or(SurfaceSessionId(1))
    }

    fn pane_for_toplevel_surface(&self, surface: &WlSurface) -> Option<u64> {
        self.window_pane_ids
            .iter()
            .rev()
            .find_map(|(assigned_surface, pane)| (assigned_surface == surface).then_some(*pane))
    }

    fn surface_for_pane(&self, target_pane: u64) -> Option<WlSurface> {
        self.window_pane_ids
            .iter()
            .rev()
            .find_map(|(surface, pane)| (*pane == target_pane).then(|| surface.clone()))
    }

    fn content_surface_for_pane(&self, pane: u64) -> Option<WlSurface> {
        let surface = self.surface_for_pane(pane)?;
        self.window_for_any_toplevel_surface(&surface)
            .is_some()
            .then_some(surface)
    }

    pub const fn active_surface_session(&self) -> SurfaceSessionId {
        self.sessions.active()
    }

    pub fn restore_last_interacted_surface_session(&mut self) -> bool {
        let Some(session) = self.most_recent_live_surface_session() else {
            return false;
        };
        if self.sessions.active() == session {
            return false;
        }
        self.stop_configured_workspace_plugins_for_rebind();
        if self.sessions.activate_recent(session).is_err() {
            return false;
        }
        self.reconfigure_focus_stack();
        self.start_configured_workspace_plugins_after_rebind();
        true
    }

    fn most_recent_live_surface_session(&self) -> Option<SurfaceSessionId> {
        self.sessions
            .interaction_order()
            .iter()
            .copied()
            .find(|session| {
                self.sessions.contains(*session)
                    && self.space.elements().any(|window| {
                        let Some(surface) = Self::window_toplevel_surface(window) else {
                            return false;
                        };
                        self.session_for_toplevel_surface(&surface)
                            .unwrap_or(SurfaceSessionId(1))
                            == *session
                    })
            })
    }

    fn record_surface_session_interaction(&mut self, session: SurfaceSessionId) {
        self.sessions.record_interaction(session);
    }

    pub fn reset_transient_input_state(&mut self) {
        for key_code in std::mem::take(&mut self.forwarded_pressed_keys)
            .into_iter()
            .rev()
        {
            self.seat.get_keyboard().expect("keyboard").input::<(), _>(
                self,
                key_code,
                smithay::backend::input::KeyState::Released,
                SERIAL_COUNTER.next_serial(),
                0,
                |_, _, _| smithay::input::keyboard::FilterResult::Forward,
            );
        }

        self.ctrl_forwarded_to_client = false;
        self.alt_forwarded_to_client = false;
        self.shift_forwarded_to_client = false;
        self.focus.clear_transient_pointer_state();
    }

    pub const fn surface_session_count(&self) -> usize {
        self.sessions.count()
    }

    pub fn clear_initial_surface_session(&mut self) {
        if self.sessions.clear_initial().is_err() {
            warn!("refusing to clear non-initial surface session state");
        }
    }

    pub fn surface_session_summaries(&self) -> Vec<SurfaceSessionSummary> {
        self.sessions
            .sessions()
            .iter()
            .copied()
            .filter_map(|session| self.surface_session_summary(session))
            .collect()
    }

    pub fn surface_session_summary(
        &self,
        session: SurfaceSessionId,
    ) -> Option<SurfaceSessionSummary> {
        self.sessions.contains(session).then(|| {
            let project = self.sessions.project(session);
            SurfaceSessionSummary {
                id: session.raw(),
                name: self.surface_session_name(session).to_string(),
                active: session == self.sessions.active(),
                windows: self.surface_session_window_count(session),
                working_directory: self
                    .sessions
                    .working_directory(session)
                    .map(|directory| directory.to_string_lossy().into_owned()),
                project_id: project.map_or_else(String::new, |project| project.id.clone()),
                project_name: project
                    .map_or_else(|| "Project".to_string(), |project| project.name.clone()),
                project_directory: project.map_or_else(String::new, |project| {
                    project.source_directory.to_string_lossy().into_owned()
                }),
                branch: project.and_then(|project| project.branch.clone()),
                worktree: project.is_some_and(|project| project.is_worktree),
                git: project.is_some_and(|project| project.is_git),
                isolation: self
                    .sessions
                    .isolation_status(session)
                    .cloned()
                    .unwrap_or_default(),
            }
        })
    }

    pub fn rename_surface_session(&mut self, session: SurfaceSessionId, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        self.sessions.rename(session, name).is_ok()
    }

    pub fn active_surface_session_name(&self) -> &str {
        self.surface_session_name(self.sessions.active())
    }

    pub fn resolve_directory_surface_session(
        &mut self,
        directory: &Path,
    ) -> Option<SurfaceSessionId> {
        self.prune_directory_surface_sessions(directory);
        self.sessions.directory_default(directory)
    }

    pub fn directory_surface_session_summaries(
        &mut self,
        directory: &Path,
    ) -> Vec<SurfaceSessionSummary> {
        self.prune_directory_surface_sessions(directory);
        self.sessions
            .directory_sessions(directory)
            .iter()
            .copied()
            .filter_map(|session| self.surface_session_summary(session))
            .collect()
    }

    pub fn remember_directory_surface_session(
        &mut self,
        directory: PathBuf,
        session: SurfaceSessionId,
        set_default: bool,
    ) -> bool {
        self.sessions
            .remember_directory(directory, session, set_default)
            .is_ok()
    }

    fn prune_directory_surface_sessions(&mut self, directory: &Path) {
        self.sessions.prune_directory(directory);
    }

    pub fn close_surface_session(&mut self, session: SurfaceSessionId) -> bool {
        if !self.sessions.contains(session) {
            return false;
        }
        let closing_active = self.sessions.active() == session;
        if closing_active {
            self.stop_configured_workspace_plugins_for_rebind();
        }

        let surfaces = self.content_surfaces_for_session(session);
        for surface in &surfaces {
            self.send_close_to_toplevel_surface(surface);
        }

        self.remove_surface_session_record(session);
        self.reconfigure_focus_stack();
        if closing_active {
            self.start_configured_workspace_plugins_after_rebind();
        }
        if !surfaces.is_empty()
            && let Err(err) = self.display_handle.flush_clients()
        {
            warn!(?err, "flush clients after surface session close failed");
        }
        true
    }

    fn remove_surface_session_record(&mut self, session: SurfaceSessionId) {
        if self.backend_init.is_some() {
            match serde_json::to_vec(&serde_json::json!({
                "kind": "released",
                "session": session.raw(),
                "now": unix_timestamp(),
            })) {
                Ok(body) => {
                    if let Err(error) = self.send_backend_actor_message("host:session", body) {
                        warn!(error = %error, "failed to release backend worktree occupation");
                    }
                }
                Err(error) => {
                    warn!(error = %error, "failed to encode backend worktree release");
                }
            }
        }
        if let Err(error) = self.stop_session_init_actor(session) {
            warn!(surface_session = session.raw(), error = %error, "failed to stop surface-session init actor");
        }
        let _ = self.sessions.remove(session);
    }

    fn content_surfaces_for_session(&self, session: SurfaceSessionId) -> Vec<WlSurface> {
        self.sessions
            .surface_assignments()
            .iter()
            .filter(|(surface, assigned_session)| {
                *assigned_session == session && self.pane_for_toplevel_surface(surface).is_some()
            })
            .map(|(surface, _assigned_session)| surface.clone())
            .collect()
    }

    pub fn resolve_surface_session(&self, query: &str) -> Option<SurfaceSessionId> {
        self.sessions.resolve(query)
    }

    pub fn surface_session_name(&self, session: SurfaceSessionId) -> &str {
        self.sessions.name(session).unwrap_or("untitled")
    }

    pub fn active_surface_session_window_count(&self) -> usize {
        self.surface_session_window_count(self.sessions.active())
    }

    fn surface_session_window_count(&self, session: SurfaceSessionId) -> usize {
        self.space
            .elements()
            .filter(|window| {
                let Some(surface) = Self::window_toplevel_surface(window) else {
                    return false;
                };
                let assigned_session = self
                    .session_for_toplevel_surface(&surface)
                    .unwrap_or(SurfaceSessionId(1));
                assigned_session == session
            })
            .count()
    }

    pub fn switch_to_surface_session(&mut self, session: SurfaceSessionId) -> bool {
        if !self.sessions.contains(session) || self.sessions.active() == session {
            return false;
        }
        self.stop_configured_workspace_plugins_for_rebind();
        if self.sessions.activate(session).is_err() {
            return false;
        }
        self.reconfigure_focus_stack();
        self.start_configured_workspace_plugins_after_rebind();
        true
    }

    pub fn switch_to_next_surface_session(&mut self) -> bool {
        if self.sessions.count() <= 1 {
            return false;
        }
        self.stop_configured_workspace_plugins_for_rebind();
        let Ok(_next) = self.sessions.activate_next() else {
            self.start_configured_workspace_plugins_after_rebind();
            return false;
        };
        self.reconfigure_focus_stack();
        self.start_configured_workspace_plugins_after_rebind();
        true
    }

    pub fn create_and_attach_surface_session(&mut self) -> Result<SurfaceSessionId> {
        self.create_and_attach_named_surface_session(None)
    }

    pub fn create_and_attach_named_surface_session(
        &mut self,
        name: Option<String>,
    ) -> Result<SurfaceSessionId> {
        self.create_and_attach_named_surface_session_in_directory(name, current_process_directory())
    }

    pub fn create_and_attach_named_surface_session_in_directory(
        &mut self,
        name: Option<String>,
        directory: PathBuf,
    ) -> Result<SurfaceSessionId> {
        self.stop_configured_workspace_plugins_for_rebind();
        let next = self.sessions.next_id();
        let session = self.sessions.create(
            name.filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| default_surface_session_name(next)),
            directory,
        )?;
        self.reconfigure_focus_stack();
        self.start_configured_workspace_plugins_after_rebind();
        if self.session_init_actor.is_some() {
            self.start_session_init_actor(session)?;
        }
        Ok(session)
    }

    fn session_for_toplevel_surface(&self, surface: &WlSurface) -> Option<SurfaceSessionId> {
        self.sessions.surface_session(surface)
    }

    pub fn window_belongs_to_active_surface_session(&self, window: &Window) -> bool {
        let Some(surface) = Self::window_toplevel_surface(window) else {
            return false;
        };
        self.session_for_toplevel_surface(&surface)
            .unwrap_or(SurfaceSessionId(1))
            == self.sessions.active()
    }

    pub(crate) fn window_available_to_presentation_client(&self, window: &Window) -> bool {
        let Some(surface) = Self::window_toplevel_surface(window) else {
            return false;
        };
        self.pane_for_toplevel_surface(&surface).is_some()
    }

    fn presentation_window_count(&self) -> usize {
        self.space
            .elements()
            .filter(|window| self.window_available_to_presentation_client(window))
            .count()
    }

    pub(crate) fn active_presentation_client_window(&self) -> Option<Window> {
        self.focus.active_toplevel().and_then(|active| {
            self.window_for_any_toplevel_surface(active)
                .filter(|window| self.window_available_to_presentation_client(window))
        })
    }

    fn surface_can_receive_focus(&self, surface: &WlSurface) -> bool {
        self.pane_for_toplevel_surface(surface).is_some()
    }

    pub(crate) fn window_pane_id(&self, window: &Window) -> Option<u64> {
        let surface = Self::window_toplevel_surface(window)?;
        self.pane_for_toplevel_surface(&surface)
    }

    pub(crate) fn window_app_id(&self, window: &Window) -> Option<String> {
        let surface = Self::window_toplevel_surface(window)?;
        toplevel_app_id(&surface)
    }

    pub(crate) fn window_title(&self, window: &Window) -> Option<String> {
        let surface = Self::window_toplevel_surface(window)?;
        toplevel_title(&surface)
    }

    pub(crate) fn window_session_id(&self, window: &Window) -> Option<u64> {
        let surface = Self::window_toplevel_surface(window)?;
        self.session_for_toplevel_surface(&surface)
            .map(SurfaceSessionId::raw)
    }

    pub fn presentation_window_summaries(&self) -> Vec<PresentationWindowSummary> {
        let mut windows = self
            .space
            .elements()
            .filter_map(|window| {
                let surface = Self::window_toplevel_surface(window)?;
                let pane = self.pane_for_toplevel_surface(&surface)?;
                let session = self
                    .session_for_toplevel_surface(&surface)
                    .unwrap_or_else(|| self.default_surface_session())
                    .raw();
                Some(PresentationWindowSummary {
                    pane,
                    session,
                    app_id: toplevel_app_id(&surface).unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>();
        windows.sort_by_key(|window| window.pane);
        windows
    }

    pub(crate) fn presentation_window_for_pane(&self, pane: u64) -> Option<Window> {
        let surface = self.surface_for_pane(pane)?;
        // Presentation clients own visibility across the complete window
        // inventory. Session membership is metadata, not a mapping boundary.
        let window = self.window_for_any_toplevel_surface(&surface)?;
        self.window_available_to_presentation_client(&window)
            .then_some(window)
    }

    pub(crate) fn presentation_surface_under_pane_local(
        &self,
        pane: u64,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let window = self.presentation_window_for_pane(pane)?;
        let surface_origin =
            window_surface_origin_in_compositor_space(Point::from((0, 0)), window.geometry().loc);
        let local_pos = Point::from((
            pos.x - f64::from(surface_origin.x),
            pos.y - f64::from(surface_origin.y),
        ));
        window
            .surface_under(local_pos, WindowSurfaceType::ALL)
            .map(|(surface, point)| (surface, (surface_origin + point).to_f64()))
    }

    pub(crate) fn window_test_marker_id(&self, window: &Window, fallback_index: usize) -> u64 {
        let Some(surface) = Self::window_toplevel_surface(window) else {
            return fallback_marker_id(fallback_index);
        };
        self.pane_for_toplevel_surface(&surface)
            .unwrap_or_else(|| fallback_marker_id(fallback_index))
    }

    pub fn request_toplevel_fullscreen(
        &mut self,
        surface: WlSurface,
        output: Option<wl_output::WlOutput>,
    ) {
        self.fullscreen_toplevels
            .retain(|(fullscreen_surface, _)| fullscreen_surface != &surface);
        self.fullscreen_toplevels.push((surface.clone(), output));
        self.focus_toplevel_surface(Some(surface));
        self.reconfigure_focus_stack();
    }

    pub fn request_toplevel_unfullscreen(&mut self, surface: &WlSurface) {
        self.fullscreen_toplevels
            .retain(|(fullscreen_surface, _)| fullscreen_surface != surface);
        self.reconfigure_focus_stack();
    }

    pub const fn requested_fullscreen_count(&self) -> usize {
        self.fullscreen_toplevels.len()
    }

    pub fn focused_window_fullscreen(&self) -> bool {
        self.focus
            .active_toplevel()
            .is_some_and(|surface| self.toplevel_fullscreen_output(surface).is_some())
    }

    fn toplevel_fullscreen_output(
        &self,
        surface: &WlSurface,
    ) -> Option<&Option<wl_output::WlOutput>> {
        self.fullscreen_toplevels
            .iter()
            .rev()
            .find_map(|(fullscreen_surface, output)| {
                (fullscreen_surface == surface).then_some(output)
            })
    }

    pub fn focus_toplevel_surface(&mut self, active_surface: Option<WlSurface>) {
        if let Some(session) = active_surface
            .as_ref()
            .and_then(|surface| self.session_for_toplevel_surface(surface))
        {
            self.record_surface_session_interaction(session);
        }
        self.focus.set_active_toplevel(active_surface.clone());
        self.activate_windows_without_geometry(active_surface.as_ref());
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard_focus =
            active_surface.filter(|surface| self.surface_can_receive_focus(surface));
        self.seat
            .get_keyboard()
            .expect("keyboard")
            .set_focus(self, keyboard_focus, serial);
    }

    pub fn focus_pane(&mut self, pane: u64) -> bool {
        self.focus_pane_from_presentation_client(pane)
    }

    pub fn focus_pane_from_presentation_client(&mut self, pane: u64) -> bool {
        let Some(surface) = self.content_surface_for_pane(pane) else {
            return false;
        };
        let Some(window) = self.window_for_any_toplevel_surface(&surface) else {
            return false;
        };
        self.space.raise_element(&window, true);
        if let Some(session) = self.session_for_toplevel_surface(&surface) {
            self.record_surface_session_interaction(session);
        }
        self.focus.set_active_toplevel(Some(surface.clone()));
        self.activate_windows_without_geometry(Some(&surface));
        let serial = SERIAL_COUNTER.next_serial();
        self.seat
            .get_keyboard()
            .expect("keyboard")
            .set_focus(self, Some(surface), serial);
        true
    }

    pub fn configure_presentation_client_pane_size(
        &mut self,
        pane: u64,
        width: i32,
        height: i32,
    ) -> bool {
        let Some(surface) = self.content_surface_for_pane(pane) else {
            return false;
        };
        let Some(window) = self.window_for_any_toplevel_surface(&surface) else {
            return false;
        };
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        let requested = (width.max(1), height.max(1));
        self.presentation_client_requested_sizes
            .insert(pane, requested);
        self.configure_presentation_client_toplevel_size(pane, &surface, toplevel, requested)
    }

    pub(crate) fn computer_use_owns_pane(&self, pane: u64) -> bool {
        let Some(lease) = self.computer_use.lease() else {
            return false;
        };
        self.presentation_window_summaries().iter().any(|window| {
            window.pane == pane
                && window.session == lease.session
                && (lease.windows.is_empty() || lease.windows.contains(&pane))
        })
    }

    pub(crate) fn computer_use_pane_is_unlocked(&self, pane: u64) -> bool {
        let Some(lease) = self.computer_use.lease() else {
            return false;
        };
        self.computer_use_owns_pane(pane)
            && self
                .computer_use_automation
                .lease_is_unlocked(lease.generation)
    }

    pub(crate) fn computer_use_automation_state(
        &self,
        pane: u64,
    ) -> brood_protocol::ManagedWindowAutomationState {
        let Some(lease) = self
            .computer_use
            .lease()
            .filter(|_| self.computer_use_owns_pane(pane))
        else {
            return brood_protocol::ManagedWindowAutomationState::Normal;
        };
        let interaction = if self
            .computer_use_automation
            .lease_is_unlocked(lease.generation)
        {
            brood_protocol::AutomationInteractionState::Unlocked
        } else {
            brood_protocol::AutomationInteractionState::Locked
        };
        let cursor = self
            .computer_use_automation
            .cursor_position(pane)
            .map(|(x, y)| brood_protocol::AutomationCursor { x, y });
        brood_protocol::ManagedWindowAutomationState::Automated {
            task: lease.task.0.clone(),
            interaction,
            cursor,
        }
    }

    pub(crate) fn configure_automation_pane_size(
        &mut self,
        pane: u64,
        width: i32,
        height: i32,
    ) -> bool {
        let Some(surface) = self.content_surface_for_pane(pane) else {
            return false;
        };
        let Some(window) = self.window_for_any_toplevel_surface(&surface) else {
            return false;
        };
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        self.configure_presentation_client_toplevel_size(
            pane,
            &surface,
            toplevel,
            (width.max(1), height.max(1)),
        )
    }

    pub fn restart_ordinary_presentation_client_pane_size(
        &mut self,
        fast_path: &mut PresentationFastPath,
        pane: u64,
        width: i32,
        height: i32,
    ) -> bool {
        match fast_path.managed_resizes.decide_ordinary_resize(pane, None) {
            OrdinaryManagedResizeDecision::Configure => {}
            OrdinaryManagedResizeDecision::IgnoreUnscoped => {
                info!(
                    pane,
                    "ignored unscoped ordinary resize during tagged authority"
                );
                return false;
            }
            OrdinaryManagedResizeDecision::IgnoreStaleRecovery
            | OrdinaryManagedResizeDecision::RecoverThrough(_) => return false,
        }
        self.configure_presentation_client_pane_size(pane, width, height)
    }

    pub fn recover_ordinary_presentation_client_pane_size(
        &mut self,
        fast_path: &mut PresentationFastPath,
        pane: u64,
        width: i32,
        height: i32,
        recover_through_epoch: u64,
    ) -> bool {
        if !managed_resize_epoch_is_valid(recover_through_epoch) {
            return false;
        }
        // Validate the complete configure target before retiring authority.
        // Once this succeeds, send_configure below is infallible and always
        // emits a fresh serial, including when the negotiated size is already
        // current. That fresh serial is the causal source of the commit floor.
        let Some(surface) = self.content_surface_for_pane(pane) else {
            return false;
        };
        let Some(window) = self.window_for_any_toplevel_surface(&surface) else {
            return false;
        };
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        let requested = (width.max(1), height.max(1));
        let constraints = with_states(&surface, |states| {
            *states.cached_state.get::<SurfaceCachedState>().current()
        });
        let (configured_size, negotiation) = negotiate_window_size(
            requested,
            constraints.min_size.into(),
            constraints.max_size.into(),
        );
        if configured_size.0 <= 0 || configured_size.1 <= 0 {
            return false;
        }
        if fast_path
            .managed_resizes
            .has_epoch_after(pane, recover_through_epoch)
        {
            info!(
                pane,
                recover_through_epoch,
                "ignored stale ordinary recovery behind newer tagged authority"
            );
            return false;
        }
        let retired = fast_path
            .managed_resizes
            .retire_pending_through_epoch(pane, recover_through_epoch);
        crate::frame_export::cleanup_provisional_tagged_resources(fast_path);
        let size: Size<i32, Logical> = configured_size.into();
        let serial = emit_presentation_toplevel_configure(
            toplevel,
            size,
            PresentationConfigurePolicy::Force,
        )
        .expect("forced presentation configure always emits one serial");
        self.presentation_client_requested_sizes
            .insert(pane, requested);
        info!(
            pane,
            recover_through_epoch,
            serial = u32::from(serial),
            requested_width = requested.0,
            requested_height = requested.1,
            configured_width = configured_size.0,
            configured_height = configured_size.1,
            ?negotiation,
            "ordinary managed recovery configure issued"
        );
        // The configure response cannot commit synchronously. Arm after the
        // configure was accepted and before returning to the control loop,
        // which requests redraw for this successful operation.
        let recovery_floor = fast_path
            .managed_resizes
            .wait_for_ordinary_recovery_commit(pane);
        info!(
            pane,
            recover_through_epoch,
            serial = u32::from(serial),
            recovery_floor,
            ?retired,
            "accepted and armed ordinary managed recovery"
        );
        true
    }

    pub fn configure_tagged_presentation_client_pane_size(
        &mut self,
        fast_path: &mut PresentationFastPath,
        pane: u64,
        logical_size: (i32, i32),
        pixel_size: (i32, i32),
        scale_120: u32,
        epoch: u64,
    ) -> bool {
        let reject = |fast_path: &mut PresentationFastPath,
                      reason: brood_protocol::ManagedResizeRejectionReason| {
            emit_managed_resize_status(
                fast_path,
                brood_protocol::ManagedResizeStatus {
                    pane,
                    resize_epoch: epoch,
                    result: brood_protocol::ManagedResizeStatusResult::Rejected { reason },
                },
            );
        };
        let Some(surface) = self.content_surface_for_pane(pane) else {
            reject(
                fast_path,
                brood_protocol::ManagedResizeRejectionReason::PaneUnavailable,
            );
            return false;
        };
        let Some(window) = self.window_for_any_toplevel_surface(&surface) else {
            reject(
                fast_path,
                brood_protocol::ManagedResizeRejectionReason::PaneUnavailable,
            );
            return false;
        };
        let Some(toplevel) = window.toplevel() else {
            reject(
                fast_path,
                brood_protocol::ManagedResizeRejectionReason::PaneUnavailable,
            );
            return false;
        };
        if !managed_resize_epoch_is_valid(epoch)
            || scale_120 == 0
            || logical_size.0 <= 0
            || logical_size.1 <= 0
            || pixel_size.0 <= 0
            || pixel_size.1 <= 0
            || self.current_scale_120() != scale_120
            || !pixel_size_matches_scale(logical_size, pixel_size, scale_120)
        {
            reject(
                fast_path,
                brood_protocol::ManagedResizeRejectionReason::SizeUnsatisfied,
            );
            return false;
        }
        let constraints = with_states(&surface, |states| {
            *states.cached_state.get::<SurfaceCachedState>().current()
        });
        let (configured_size, negotiation) = negotiate_window_size(
            logical_size,
            constraints.min_size.into(),
            constraints.max_size.into(),
        );
        if negotiation != WindowSizeNegotiation::Requested || configured_size != logical_size {
            reject(
                fast_path,
                brood_protocol::ManagedResizeRejectionReason::SizeUnsatisfied,
            );
            return false;
        }
        let reserved_bytes = match fast_path.managed_resizes.reserve(pane, epoch, pixel_size) {
            Ok(bytes) => bytes,
            Err(reason) => {
                reject(fast_path, reason);
                return false;
            }
        };
        crate::frame_export::cleanup_provisional_tagged_resources(fast_path);
        let size: Size<i32, Logical> = logical_size.into();
        // A tagged request always sends a configure. This preserves a distinct
        // serial when the logical size is unchanged but the epoch is new.
        let serial = emit_presentation_toplevel_configure(
            toplevel,
            size,
            PresentationConfigurePolicy::Force,
        )
        .expect("forced presentation configure always emits one serial");
        let sent_after_commit = fast_path.managed_resizes.current_surface_commit(pane);
        fast_path
            .managed_resizes
            .records
            .entry(pane)
            .or_default()
            .push_back(ManagedResizeCorrelation {
                epoch,
                serial,
                logical_size,
                pixel_size,
                sent_after_commit,
                reserved_bytes,
                configured_at: Instant::now(),
                state: ManagedResizeCorrelationState::ConfigureSent,
            });
        self.presentation_client_requested_sizes
            .insert(pane, logical_size);
        emit_managed_resize_status(
            fast_path,
            brood_protocol::ManagedResizeStatus {
                pane,
                resize_epoch: epoch,
                result: brood_protocol::ManagedResizeStatusResult::Configured {
                    logical_size: [
                        u32::try_from(logical_size.0).unwrap_or(u32::MAX),
                        u32::try_from(logical_size.1).unwrap_or(u32::MAX),
                    ],
                    pixel_size: [
                        u32::try_from(pixel_size.0).unwrap_or(u32::MAX),
                        u32::try_from(pixel_size.1).unwrap_or(u32::MAX),
                    ],
                    scale_120,
                },
            },
        );
        info!(
            pane,
            epoch,
            serial = u32::from(serial),
            logical_width = logical_size.0,
            logical_height = logical_size.1,
            pixel_width = pixel_size.0,
            pixel_height = pixel_size.1,
            scale_120,
            "managed resize configure issued"
        );
        true
    }

    pub(crate) fn renegotiate_presentation_client_pane_size(
        &mut self,
        surface: &WlSurface,
    ) -> bool {
        let Some(pane) = self.pane_for_toplevel_surface(surface) else {
            return false;
        };
        if self.computer_use_owns_pane(pane) {
            info!(
                pane,
                "suppressed presentation pane-size renegotiation during computer use"
            );
            return false;
        }
        let active_correlation = self
            .dmabuf
            .fast_path
            .as_ref()
            .is_some_and(|fast_path| fast_path.managed_resizes.has_active(pane));
        if managed_resize_suppresses_ordinary_renegotiation(
            self.dmabuf.fast_path_input_draining,
            active_correlation,
        ) {
            info!(
                pane,
                input_draining = self.dmabuf.fast_path_input_draining,
                active_correlation,
                "suppressed ordinary pane-size renegotiation during managed resize"
            );
            return false;
        }
        let Some(requested) = self.presentation_client_requested_sizes.get(&pane).copied() else {
            return false;
        };
        let Some(window) = self.window_for_any_toplevel_surface(surface) else {
            return false;
        };
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        self.configure_presentation_client_toplevel_size(pane, surface, toplevel, requested)
    }

    fn configure_presentation_client_toplevel_size(
        &self,
        pane: u64,
        surface: &WlSurface,
        toplevel: &smithay::wayland::shell::xdg::ToplevelSurface,
        requested: (i32, i32),
    ) -> bool {
        let constraints = with_states(surface, |states| {
            *states.cached_state.get::<SurfaceCachedState>().current()
        });
        let (size, negotiation) = negotiate_window_size(
            requested,
            constraints.min_size.into(),
            constraints.max_size.into(),
        );
        let size: Size<i32, Logical> = size.into();
        let already_configured = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|data| data.lock().ok())
                .is_some_and(|data| {
                    let current = data.current_server_state();
                    presentation_toplevel_state_matches(current, size)
                })
        });
        if !PresentationConfigurePolicy::Deduplicate.should_emit(already_configured) {
            return false;
        }
        let configured = emit_presentation_toplevel_configure(
            toplevel,
            size,
            PresentationConfigurePolicy::Deduplicate,
        )
        .is_some();
        if configured && negotiation != WindowSizeNegotiation::Requested {
            info!(
                pane,
                requested_width = requested.0,
                requested_height = requested.1,
                minimum_width = constraints.min_size.w,
                minimum_height = constraints.min_size.h,
                maximum_width = constraints.max_size.w,
                maximum_height = constraints.max_size.h,
                configured_width = size.w,
                configured_height = size.h,
                ?negotiation,
                "negotiated presentation-client pane size"
            );
        }
        configured
    }

    fn current_scale_120(&self) -> u32 {
        scale_120_for_fractional(self.current_fractional_scale())
    }

    pub(crate) fn record_managed_commit_observation(
        &mut self,
        surface: &WlSurface,
        sample_size: Option<(i32, i32)>,
    ) {
        let Some(pane) = self.pane_for_toplevel_surface(surface) else {
            return;
        };
        // Smithay advances `current_serial` only after pre-commit hooks. The
        // configure serial is the exact ACK that this pending buffer commits.
        let acknowledged_serial = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|data| data.lock().ok())
                .and_then(|data| data.configure_serial)
        });
        let Some((fast_path, serial)) = self.dmabuf.fast_path.as_mut().zip(acknowledged_serial)
        else {
            return;
        };
        if !fast_path.managed_resizes.has_active_serial(pane, serial) {
            return;
        }
        let recorded = sample_size.is_some_and(|sample_size| {
            fast_path
                .managed_resizes
                .record_commit_observation(pane, serial, sample_size)
        });
        info!(
            pane,
            serial = u32::from(serial),
            ?sample_size,
            recorded,
            "observed managed resize pre-commit buffer"
        );
    }

    pub(crate) fn capture_managed_resize_commit(&mut self, surface: &WlSurface) {
        let Some(pane) = self.pane_for_toplevel_surface(surface) else {
            return;
        };
        let Some(window) = self.window_for_any_toplevel_surface(surface) else {
            return;
        };
        let Some(fast_path) = self.dmabuf.fast_path.as_mut() else {
            return;
        };
        let surface_commit = fast_path.managed_resizes.next_surface_commit(pane);
        let current_serial = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|data| data.lock().ok())
                .and_then(|data| data.current_serial)
        });
        let new_buffer_size = current_serial.and_then(|serial| {
            fast_path
                .managed_resizes
                .take_commit_observation(pane, serial)
        });
        // XDG configures describe window geometry, excluding client shadows.
        // Match the same clamped geometry that Space uses when capturing the
        // window; the root surface can legitimately be larger (e.g. Chromium).
        let geometry = window.geometry();
        let window_size = Some((geometry.size.w, geometry.size.h));
        let candidate = fast_path
            .managed_resizes
            .records
            .get_mut(&pane)
            .and_then(|records| {
                records.iter_mut().rev().find(|record| {
                    Some(record.serial) == current_serial
                        && matches!(
                            record.state,
                            ManagedResizeCorrelationState::ConfigureSent
                                | ManagedResizeCorrelationState::CurrentUnsuitable
                        )
                })
            });
        let Some(candidate) = candidate else {
            return;
        };
        let suitable = managed_commit_is_suitable(
            candidate,
            current_serial,
            surface_commit,
            window_size,
            new_buffer_size,
        );
        if !suitable {
            candidate.state = ManagedResizeCorrelationState::CurrentUnsuitable;
            info!(
                pane,
                epoch = candidate.epoch,
                serial = u32::from(candidate.serial),
                surface_commit,
                has_new_buffer = new_buffer_size.is_some(),
                ?window_size,
                sample_size = ?new_buffer_size,
                expected_logical_size = ?candidate.logical_size,
                expected_pixel_size = ?candidate.pixel_size,
                "managed resize commit is not suitable"
            );
            return;
        }
        let epoch = candidate.epoch;
        let pixel_size = candidate.pixel_size;
        let configured_at = candidate.configured_at;
        let serial = candidate.serial;
        let snapshot =
            crate::frame_export::capture_managed_commit_snapshot(self, &window, pane, pixel_size);
        let Some(candidate) = self
            .dmabuf
            .fast_path
            .as_mut()
            .and_then(|fast_path| fast_path.managed_resizes.records.get_mut(&pane))
            .and_then(|records| {
                records.iter_mut().find(|record| {
                    record.epoch == epoch
                        && record.serial == serial
                        && matches!(
                            record.state,
                            ManagedResizeCorrelationState::ConfigureSent
                                | ManagedResizeCorrelationState::CurrentUnsuitable
                        )
                })
            })
        else {
            return;
        };
        match snapshot {
            Ok(backing) => {
                candidate.state = ManagedResizeCorrelationState::SnapshotQueued {
                    surface_commit,
                    committed_at: Instant::now(),
                    backing,
                };
                info!(
                    pane,
                    epoch,
                    serial = u32::from(serial),
                    surface_commit,
                    logical_width = candidate.logical_size.0,
                    logical_height = candidate.logical_size.1,
                    pixel_width = candidate.pixel_size.0,
                    pixel_height = candidate.pixel_size.1,
                    configure_to_snapshot_micros = configured_at.elapsed().as_micros(),
                    "managed resize snapshot queued"
                );
            }
            Err(error) => {
                candidate.state = ManagedResizeCorrelationState::CurrentUnsuitable;
                warn!(
                    pane,
                    epoch,
                    serial = u32::from(serial),
                    surface_commit,
                    ?error,
                    "managed resize snapshot capture failed"
                );
            }
        }
    }

    pub fn active_window(&self) -> Option<Window> {
        self.focus.active_toplevel().and_then(|active| {
            self.space
                .elements()
                .find(|window| {
                    self.window_available_to_presentation_client(window)
                        && Self::window_toplevel_surface(window).as_ref() == Some(active)
                })
                .cloned()
        })
    }

    pub fn focus_window(&mut self, window: &Window) {
        self.space.raise_element(window, true);
        let Some(surface) = Self::window_toplevel_surface(window) else {
            self.focus_toplevel_surface(None);
            return;
        };
        if let Some(pane) = self.pane_for_toplevel_surface(&surface)
            && self.focus_pane(pane)
        {
            return;
        }
        self.focus_toplevel_surface(Some(surface));
    }

    pub fn focus_app_id(&mut self, target_app_id: &str) -> bool {
        let window = self
            .space
            .elements()
            .filter(|window| self.window_belongs_to_active_surface_session(window))
            .find(|window| {
                window
                    .toplevel()
                    .and_then(|toplevel| toplevel_app_id(toplevel.wl_surface()))
                    .as_deref()
                    == Some(target_app_id)
            })
            .cloned();
        let Some(window) = window else {
            return false;
        };
        self.focus_window(&window);
        true
    }

    pub fn focus_window_identity(&mut self, identity: &ExtensionMetadata) -> bool {
        let window = self
            .space
            .elements()
            .filter(|window| self.window_belongs_to_active_surface_session(window))
            .find(|window| {
                Self::window_toplevel_surface(window).is_some_and(|surface| {
                    toplevel_matches_agent_identity(self, &surface, identity)
                })
            })
            .cloned();
        let Some(window) = window else {
            return false;
        };
        self.focus_window(&window);
        true
    }

    pub fn set_presentation_window_focused(&mut self, focused: bool) {
        if self.focus.set_presentation_window_focused(focused) {
            self.reset_transient_input_state();
        }
    }

    pub const fn presentation_window_focused(&self) -> bool {
        self.focus.presentation_window_focused()
    }

    pub fn agent_has_focused_window(&self, agent_id: &str) -> bool {
        let Some(record) = self
            .extension_registry
            .agents
            .iter()
            .find(|record| record.id == agent_id)
        else {
            return false;
        };
        if record.session != self.sessions.active() {
            return false;
        }
        let Some(surface) = self.focus.active_toplevel() else {
            return false;
        };
        if self
            .session_for_toplevel_surface(surface)
            .unwrap_or(SurfaceSessionId(1))
            != record.session
        {
            return false;
        }
        toplevel_matches_agent_identity(self, surface, &record.metadata)
    }

    pub fn should_notify_agent_desktop(&self, agent_id: &str) -> bool {
        !self.presentation_window_focused() || !self.agent_has_focused_window(agent_id)
    }

    pub fn focus_presentation_window(&mut self) -> bool {
        self.focus_presentation_window_with_activation_token("")
    }

    pub fn focus_presentation_window_with_activation_token(
        &mut self,
        _activation_token: &str,
    ) -> bool {
        let Some(fast_path) = self.dmabuf.fast_path.as_mut() else {
            return false;
        };
        if fast_path
            .frames
            .try_send(brood_ipc::CriticalRecord {
                opcode: brood_ipc::FRAME_FOCUS_PRESENTATION,
                connection_generation: fast_path.generation,
                ..brood_ipc::CriticalRecord::default()
            })
            .is_err()
        {
            return false;
        }
        fast_path.frame_notifications.notify().is_ok()
    }

    pub fn close_presentation_client(&mut self) -> bool {
        let Some(fast_path) = self.dmabuf.fast_path.as_mut() else {
            return false;
        };
        if fast_path
            .frames
            .try_send(brood_ipc::CriticalRecord {
                opcode: brood_ipc::FRAME_CLOSE_PRESENTATION,
                connection_generation: fast_path.generation,
                ..brood_ipc::CriticalRecord::default()
            })
            .is_err()
        {
            return false;
        }
        fast_path.frame_notifications.notify().is_ok()
    }

    pub fn focus_app_id_or_spawn(&mut self, app_id: &str, argv: &[String]) -> bool {
        if self.focus_app_id(app_id) {
            return true;
        }
        match self.spawn_hosted_app_argv(argv) {
            Ok(_) => true,
            Err(err) => {
                warn!(
                    ?err,
                    app_id,
                    ?argv,
                    "failed to spawn app after app-id focus miss"
                );
                false
            }
        }
    }

    pub fn register_extension(
        &mut self,
        role: &str,
        metadata: ExtensionMetadata,
        sender: String,
    ) -> Result<String> {
        let role = normalize_registry_value(role).context("extension role must not be empty")?;
        ensure_metadata(&metadata)?;
        let id = stable_registry_id("ext", [&role], &metadata);
        if let Some(record) = self
            .extension_registry
            .extensions
            .iter_mut()
            .find(|record| record.id == id)
        {
            record.role = role;
            record.metadata = metadata;
            record.sender = sender;
        } else {
            self.extension_registry.extensions.push(ExtensionRecord {
                id: id.clone(),
                role,
                metadata,
                sender,
            });
        }
        Ok(id)
    }

    pub fn unregister_extension(&mut self, extension_id: &str) -> bool {
        let before = self.extension_registry.extensions.len();
        self.extension_registry
            .extensions
            .retain(|extension| extension.id != extension_id);
        if before == self.extension_registry.extensions.len() {
            return false;
        }
        self.extension_registry
            .windows
            .retain(|window| window.extension_id != extension_id);
        self.extension_registry
            .grants
            .retain(|grant| grant.extension_id != extension_id);
        self.extension_registry
            .active_window_roles
            .retain(|_, active| active.extension_id != extension_id);
        true
    }

    pub fn extension_summaries(&self) -> Vec<ExtensionSummary> {
        self.extension_registry
            .extensions
            .iter()
            .map(|record| ExtensionSummary {
                id: record.id.clone(),
                role: record.role.clone(),
                sender: record.sender.clone(),
                metadata: record.metadata.clone(),
            })
            .collect()
    }

    pub fn grant_summaries(&self, extension_id: &str) -> Vec<GrantSummary> {
        self.extension_registry
            .grants
            .iter()
            .filter(|grant| extension_id.is_empty() || grant.extension_id == extension_id)
            .map(|grant| GrantSummary {
                id: grant.id.clone(),
                extension_id: grant.extension_id.clone(),
                kind: grant.kind.clone(),
                sender: grant.sender.clone(),
                active: grant.active,
                detail: grant.detail.clone(),
            })
            .collect()
    }

    pub fn release_grant(&mut self, grant_id: &str) -> bool {
        let Some(grant) = self
            .extension_registry
            .grants
            .iter_mut()
            .find(|grant| grant.id == grant_id && grant.active)
        else {
            return false;
        };
        grant.active = false;
        true
    }

    pub fn register_agent(
        &mut self,
        kind: &str,
        metadata: &ExtensionMetadata,
        sender: String,
    ) -> Result<String> {
        let session = metadata
            .get("surface_session")
            .or_else(|| metadata.get("session_id"))
            .and_then(|value| self.resolve_surface_session(value))
            .unwrap_or_else(|| self.sessions.active());
        self.extension_registry
            .register_agent(kind, metadata.clone(), sender, session)
    }

    pub fn update_agent(
        &mut self,
        agent_id: &str,
        status: &str,
        metadata: ExtensionMetadata,
        sender: String,
    ) -> Result<bool> {
        self.extension_registry
            .update_agent(agent_id, status, metadata, sender)
    }

    pub fn notify_agent(&mut self, request: AgentNotification) -> Result<String> {
        self.extension_registry.notify_agent(request)
    }

    pub fn clear_agent_attention(&mut self, agent_id: &str) -> bool {
        self.extension_registry.clear_agent_attention(agent_id)
    }

    pub fn unregister_agent(&mut self, agent_id: &str) -> bool {
        self.extension_registry.unregister_agent(agent_id)
    }

    pub fn unregister_agent_by_identity(
        &mut self,
        kind: &str,
        metadata: &ExtensionMetadata,
    ) -> Result<Option<String>> {
        let kind = normalize_registry_value(kind).context("agent kind must not be empty")?;
        ensure_metadata(metadata)?;
        let id = stable_agent_registry_id(&kind, metadata);
        Ok(self.unregister_agent(&id).then_some(id))
    }

    fn unregister_agents_for_pane(&mut self, pane_id: u64) -> Vec<String> {
        self.extension_registry.unregister_agents_for_pane(pane_id)
    }

    pub fn register_status_publisher(
        &mut self,
        role: &str,
        metadata: ExtensionMetadata,
        sender: String,
    ) -> Result<String> {
        let role =
            normalize_registry_value(role).context("status publisher role must not be empty")?;
        ensure_metadata(&metadata)?;
        let id = stable_registry_id("status-pub", [&role], &metadata);
        if let Some(record) = self
            .extension_registry
            .status_publishers
            .iter_mut()
            .find(|record| record.id == id)
        {
            record.role = role;
            record.metadata = metadata;
            record.sender = sender;
        } else {
            self.extension_registry
                .status_publishers
                .push(StatusPublisherRecord {
                    id: id.clone(),
                    role,
                    metadata,
                    sender,
                });
        }
        Ok(id)
    }

    pub fn publish_status(
        &mut self,
        publisher: &str,
        channel: &str,
        state: &str,
        label: &str,
        metadata: ExtensionMetadata,
    ) -> Result<bool> {
        let publisher =
            normalize_registry_value(publisher).context("status publisher must not be empty")?;
        if publisher != BROOD_STATUS_PUBLISHER {
            self.ensure_status_publisher_exists(&publisher)?;
        }
        let channel =
            normalize_registry_value(channel).context("status channel must not be empty")?;
        let state = normalize_registry_value(state).context("status state must not be empty")?;
        let label = normalize_registry_value(label).unwrap_or_else(|| channel.clone());
        ensure_metadata(&metadata)?;

        let next = StatusItemRecord {
            channel: channel.clone(),
            state,
            label,
            publisher,
            metadata,
        };
        if let Some(record) = self
            .extension_registry
            .status_items
            .iter_mut()
            .find(|record| record.channel == channel)
        {
            let changed = record != &next;
            *record = next;
            return Ok(changed);
        }
        self.extension_registry.status_items.push(next);
        Ok(true)
    }

    pub fn clear_status(&mut self, publisher: &str, channel: &str) -> bool {
        let before = self.extension_registry.status_items.len();
        self.extension_registry
            .status_items
            .retain(|item| !(item.publisher == publisher && item.channel == channel));
        before != self.extension_registry.status_items.len()
    }

    pub fn status_summaries(&self) -> Vec<StatusItemSummary> {
        self.extension_registry
            .status_items
            .iter()
            .map(|record| StatusItemSummary {
                channel: record.channel.clone(),
                state: record.state.clone(),
                label: record.label.clone(),
                publisher: record.publisher.clone(),
                metadata: record.metadata.clone(),
            })
            .collect()
    }

    pub fn agent_summaries(&self) -> Vec<AgentSummary> {
        self.extension_registry
            .agents
            .iter()
            .map(|record| {
                let notifications = self
                    .extension_registry
                    .notifications
                    .iter()
                    .filter(|notification| notification.agent_id == record.id)
                    .collect::<Vec<_>>();
                let last_notification = notifications.last().copied();
                AgentSummary {
                    id: record.id.clone(),
                    kind: record.kind.clone(),
                    status: record.status.clone(),
                    sender: record.sender.clone(),
                    session: record.session.raw(),
                    attention: record.attention,
                    unread_notifications: record.unread_notifications,
                    notification_count: u64::try_from(notifications.len()).unwrap_or(u64::MAX),
                    last_notification_title: last_notification
                        .map(|notification| notification.title.clone()),
                    last_notification_body: last_notification.map(|notification| {
                        if notification.subtitle.is_empty() {
                            notification.body.clone()
                        } else if notification.body.is_empty() {
                            notification.subtitle.clone()
                        } else {
                            format!("{}: {}", notification.subtitle, notification.body)
                        }
                    }),
                    metadata: record.metadata.clone(),
                }
            })
            .collect()
    }

    /// Configure the daemon-owned actor realm. Its root exists independently
    /// of surface sessions and receives only the managed backend data root.
    pub(crate) fn configure_backend_init_actor(&mut self, path: &Path) -> Result<()> {
        ensure!(
            self.backend_init_actor.is_none(),
            "backend init actor is already configured"
        );
        let path = path
            .canonicalize()
            .with_context(|| format!("resolve backend init actor {}", path.display()))?;
        let artifact = brood_wasm::WasmPluginArtifact::load(&path)?;
        ensure!(
            matches!(artifact, brood_wasm::WasmPluginArtifact::Component { .. }),
            "realm init actor must be a Component Model actor"
        );
        let data_root = brood_cli::project::managed_worktree_root();
        std::fs::create_dir_all(&data_root)
            .with_context(|| format!("create backend actor data root {}", data_root.display()))?;
        let filesystem = HostedPluginFilesystem {
            host_path: data_root,
            writable: true,
        };
        let lifecycle = ConfiguredPluginLifecycle {
            start_timeout: Duration::from_secs(2),
            stop_timeout: Duration::from_secs(2),
            data: BTreeMap::from([("realm".to_string(), "backend".to_string())]),
        };
        self.replace_wasm_component_actor(
            BACKEND_INIT_ACTOR_ID,
            &path,
            Some(&filesystem),
            Some(&(lifecycle.clone(), brood_wasm::PluginStartReason::Initial)),
            ConfiguredPluginResources::default(),
            brood_wasm::RuntimeIoCapabilities {
                commands: vec![
                    "cp".to_string(),
                    "find".to_string(),
                    "git".to_string(),
                    "mkdir".to_string(),
                    "rm".to_string(),
                    "stat".to_string(),
                    "test".to_string(),
                ],
                ..brood_wasm::RuntimeIoCapabilities::default()
            },
            NativeToolAccess::None,
            true,
            false,
            None,
            true,
            HostedReplacementPurpose::Generic,
        )?;
        self.configured_plugin_lifecycles
            .insert(BACKEND_INIT_ACTOR_ID.to_string(), lifecycle);
        self.backend_init_actor = Some(path.clone());
        self.backend_init = Some(RealmInitState {
            actor_id: BACKEND_INIT_ACTOR_ID.to_string(),
            root: path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            children: BTreeMap::new(),
            apps: BTreeMap::new(),
        });
        self.send_backend_actor_message(
            "host",
            serde_json::to_vec(&serde_json::json!({
                "kind": "bootstrap",
                "records": load_managed_worktree_records(),
                "home_directory": std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).to_string_lossy().into_owned()),
            }))
            .context("encode backend worktree bootstrap")?,
        )?;
        self.drain_hosted_actor_events();
        Ok(())
    }

    pub(crate) fn request_managed_worktree_session(
        &mut self,
        source_session: SurfaceSessionId,
        branch: &str,
        response: tokio::sync::oneshot::Sender<std::result::Result<u64, String>>,
    ) {
        self.begin_managed_worktree_session(
            source_session,
            branch,
            ManagedWorktreeResponse::Control(response),
        );
    }

    fn begin_managed_worktree_session(
        &mut self,
        source_session: SurfaceSessionId,
        branch: &str,
        response: ManagedWorktreeResponse,
    ) {
        let branch = branch.trim().to_string();
        let prepared = (|| {
            ensure!(
                self.backend_init.is_some(),
                "managed worktrees require a configured backend init actor"
            );
            ensure!(
                self.sessions.contains(source_session),
                "source session {} is unavailable",
                source_session.raw()
            );
            ensure!(!branch.is_empty(), "branch name cannot be empty");
            let source_directory = self
                .sessions
                .working_directory(source_session)
                .context("source session has no project directory")?;
            let project = brood_cli::project::discover_project(source_directory);
            ensure!(
                project.is_git,
                "project {} is not a Git repository",
                project.name
            );
            let target = brood_cli::project::managed_worktree_directory(&project, &branch);
            let data_root = brood_cli::project::managed_worktree_root();
            let relative = target.strip_prefix(&data_root).with_context(|| {
                format!(
                    "managed worktree {} is outside {}",
                    target.display(),
                    data_root.display()
                )
            })?;
            let metadata_name = format!(
                "{}.brood-worktree.json",
                relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("managed worktree path has no UTF-8 name")?
            );
            let metadata_path = relative
                .parent()
                .context("managed worktree path has no project directory")?
                .join(metadata_name);
            let request = self.next_managed_worktree_request;
            self.next_managed_worktree_request =
                self.next_managed_worktree_request.saturating_add(1);
            let message = serde_json::to_vec(&serde_json::json!({
                "kind": "acquire",
                "request": request,
                "project_id": project.id,
                "project_name": project.name,
                "source_directory": project.source_directory,
                "source_worktree": project.working_tree,
                "branch": branch,
                "target": target,
                "metadata_path": metadata_path,
                "now": unix_timestamp(),
            }))
            .context("encode managed worktree request")?;
            Ok::<_, anyhow::Error>((request, project, target, message))
        })();
        let (request, project, target, message) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.complete_managed_worktree_response(response, Err(format!("{error:#}")));
                return;
            }
        };
        self.pending_managed_worktrees.insert(
            request,
            PendingManagedWorktree {
                project,
                branch,
                target,
                response,
            },
        );
        if let Err(error) = self.send_backend_actor_message("host:control", message)
            && let Some(pending) = self.pending_managed_worktrees.remove(&request)
        {
            self.complete_managed_worktree_response(pending.response, Err(format!("{error:#}")));
        }
    }

    fn send_backend_actor_message(&self, sender: &str, body: Vec<u8>) -> Result<()> {
        let init = self
            .backend_init
            .as_ref()
            .context("backend init actor is not running")?;
        let actor = self
            .wasm_io_actors
            .get(&init.actor_id)
            .context("backend init actor generation is unavailable")?;
        let brood_wasm::HostedWasmActor::Component(actor) = actor else {
            anyhow::bail!("backend init actor is not a component actor");
        };
        actor.try_send_message(sender.to_string(), body)
    }

    fn handle_backend_actor_host_message(&mut self, payload: &[u8]) {
        let message = match serde_json::from_slice::<BackendActorHostMessage>(payload) {
            Ok(message) => message,
            Err(error) => {
                warn!(error = %error, "backend actor emitted invalid host message");
                return;
            }
        };
        match message.message_type.as_str() {
            "worktree-session-requested" => {
                let (Some(request), Some(source_session), Some(branch), Some(actor)) = (
                    message.request,
                    message.source_session,
                    message.branch,
                    message.presentation_actor,
                ) else {
                    warn!("backend actor worktree-session request is incomplete");
                    return;
                };
                self.begin_managed_worktree_session(
                    SurfaceSessionId::from_raw(source_session),
                    &branch,
                    ManagedWorktreeResponse::Presentation { actor, request },
                );
            }
            "worktree-provisioned" => {
                let Some(request) = message.request else {
                    warn!("backend actor provision response has no request ID");
                    return;
                };
                let Some(path) = message.path else {
                    self.fail_managed_worktree_request(
                        request,
                        "backend actor provision response has no path",
                    );
                    return;
                };
                self.finish_managed_worktree_request(request, Path::new(&path));
            }
            "worktree-failed" => {
                let Some(request) = message.request else {
                    warn!("backend actor failure response has no request ID");
                    return;
                };
                self.fail_managed_worktree_request(
                    request,
                    message
                        .message
                        .as_deref()
                        .unwrap_or("worktree actor failed"),
                );
            }
            "worktree-maintenance-requested" => {
                let body = serde_json::to_vec(&serde_json::json!({
                    "kind": "maintenance",
                    "now": unix_timestamp(),
                }));
                match body {
                    Ok(body) => {
                        if let Err(error) =
                            self.send_backend_actor_message("host:maintenance", body)
                        {
                            warn!(error = %error, "failed to service backend maintenance tick");
                        }
                    }
                    Err(error) => {
                        warn!(error = %error, "failed to encode backend maintenance tick");
                    }
                }
            }
            other => {
                warn!(message_type = %other, "backend actor emitted unknown host message");
            }
        }
    }

    fn finish_managed_worktree_request(&mut self, request: u64, path: &Path) {
        let Some(pending) = self.pending_managed_worktrees.remove(&request) else {
            warn!(
                request,
                "backend actor completed an unknown worktree request"
            );
            return;
        };
        if path != pending.target {
            self.complete_managed_worktree_response(
                pending.response,
                Err(format!(
                    "backend actor returned unexpected worktree path {}",
                    path.display()
                )),
            );
            return;
        }
        let result = (|| {
            let existing = self.sessions.sessions().iter().copied().find(|session| {
                self.sessions
                    .working_directory(*session)
                    .is_some_and(|directory| directory == path)
            });
            let (session, catalogue_changed) = if let Some(session) = existing {
                (session, self.switch_to_surface_session(session))
            } else {
                let session = self.create_and_attach_named_surface_session_in_directory(
                    Some(pending.branch.clone()),
                    path.to_path_buf(),
                )?;
                self.remember_directory_surface_session(path.to_path_buf(), session, true);
                (session, true)
            };
            // Worktree provisioning completes asynchronously, outside the
            // DBus request path that normally publishes session mutations.
            // Publish before delivering the actor completion so the client can
            // import and render the newly active session without requiring an
            // unrelated control request or window commit to flush the state.
            if catalogue_changed {
                crate::frame_export::request_redraw(self);
            }
            let attached = serde_json::to_vec(&serde_json::json!({
                "kind": "attached",
                "request": request,
                "session": session.raw(),
                "now": unix_timestamp(),
            }))
            .context("encode managed worktree attachment")?;
            self.send_backend_actor_message("host:session", attached)?;
            info!(
                request,
                surface_session = session.raw(),
                branch = %pending.branch,
                project = %pending.project.name,
                path = %path.display(),
                "attached managed worktree session"
            );
            Ok::<_, anyhow::Error>(session.raw())
        })();
        self.complete_managed_worktree_response(
            pending.response,
            result.map_err(|error| format!("{error:#}")),
        );
    }

    fn fail_managed_worktree_request(&mut self, request: u64, message: &str) {
        let Some(pending) = self.pending_managed_worktrees.remove(&request) else {
            warn!(request, error = %message, "backend actor failed an unknown worktree request");
            return;
        };
        self.complete_managed_worktree_response(pending.response, Err(message.to_string()));
    }

    fn fail_pending_managed_worktrees(&mut self, message: &str) {
        let pending = std::mem::take(&mut self.pending_managed_worktrees);
        for (request, pending) in pending {
            warn!(request, error = %message, "managed worktree request did not complete");
            self.complete_managed_worktree_response(pending.response, Err(message.to_string()));
        }
    }

    fn complete_managed_worktree_response(
        &mut self,
        response: ManagedWorktreeResponse,
        result: std::result::Result<u64, String>,
    ) {
        match response {
            ManagedWorktreeResponse::Control(response) => {
                let _ = response.send(result);
            }
            ManagedWorktreeResponse::Presentation { actor, request } => {
                let (success, session, message) = match result {
                    Ok(session) => (true, Some(session), String::new()),
                    Err(message) => (false, None, message),
                };
                let body = serde_json::to_vec(&serde_json::json!({
                    "type": "worktree-session-completed",
                    "request": request,
                    "success": success,
                    "session": session,
                    "message": message,
                }));
                match body {
                    Ok(body) => {
                        if let Err(error) = self.send_presentation_actor_message(
                            BackendActorRealm::Daemon,
                            "init",
                            &actor,
                            &body,
                        ) {
                            warn!(
                                actor = %actor,
                                error = %error,
                                "failed to deliver daemon worktree completion"
                            );
                        }
                    }
                    Err(error) => {
                        warn!(error = %error, "failed to encode daemon worktree completion");
                    }
                }
            }
        }
    }

    /// Configure the root artifact used for every new surface-session realm.
    pub(crate) fn configure_session_init_actor(&mut self, path: &Path) -> Result<()> {
        ensure!(
            self.session_init_actor.is_none(),
            "session init actor is already configured"
        );
        let path = path
            .canonicalize()
            .with_context(|| format!("resolve session init actor {}", path.display()))?;
        let artifact = brood_wasm::WasmPluginArtifact::load(&path)?;
        ensure!(
            matches!(artifact, brood_wasm::WasmPluginArtifact::Component { .. }),
            "realm init actor must be a Component Model actor"
        );
        self.session_init_actor = Some(path);
        let sessions = self.sessions.sessions().to_vec();
        for session in sessions {
            self.start_session_init_actor(session)?;
        }
        Ok(())
    }

    pub(crate) fn install_remote_session_init_actor(&mut self, path: &Path) -> Result<()> {
        let sessions = self.sessions.sessions().to_vec();
        for session in &sessions {
            self.stop_session_init_actor(*session)?;
        }
        self.session_init_actor = None;
        self.configure_session_init_actor(path)
    }

    pub(crate) fn take_realm_artifact_requests(&mut self) -> Vec<(u64, String)> {
        self.queued_realm_artifacts.drain(..).collect()
    }

    pub(crate) fn complete_realm_artifact_request(
        &mut self,
        request: u64,
        success: bool,
        message: String,
    ) -> bool {
        let Some(pending) = self.pending_realm_artifacts.remove(&request) else {
            return false;
        };
        if success {
            self.handle_realm_init_start(
                pending.session,
                pending.id,
                &pending.path,
                &pending.capabilities,
            );
        } else {
            self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
            self.send_realm_init_child_event(
                pending.session,
                brood_wasm::ComponentChildEvent::Failed {
                    id: pending.id,
                    message,
                },
            );
        }
        true
    }

    fn start_session_init_actor(&mut self, session: SurfaceSessionId) -> Result<()> {
        ensure!(
            self.sessions.contains(session),
            "cannot start init actor for unknown surface session {}",
            session.raw()
        );
        ensure!(
            !self.realm_inits.contains_key(&session),
            "surface session {} init actor is already running",
            session.raw()
        );
        let path = self
            .session_init_actor
            .clone()
            .context("session init actor is not configured")?;
        let actor_id = session_init_actor_id(session);
        let lifecycle = ConfiguredPluginLifecycle {
            start_timeout: Duration::from_secs(2),
            stop_timeout: Duration::from_secs(2),
            data: BTreeMap::new(),
        };
        self.replace_wasm_component_actor(
            &actor_id,
            &path,
            None,
            Some(&(lifecycle.clone(), brood_wasm::PluginStartReason::Initial)),
            ConfiguredPluginResources::default(),
            brood_wasm::RuntimeIoCapabilities::default(),
            NativeToolAccess::None,
            true,
            true,
            Some(session),
            true,
            HostedReplacementPurpose::RealmRoot { session },
        )?;
        self.configured_plugin_lifecycles
            .insert(actor_id.clone(), lifecycle);
        self.realm_inits.insert(
            session,
            RealmInitState {
                actor_id,
                root: path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf(),
                children: BTreeMap::new(),
                apps: BTreeMap::new(),
            },
        );
        self.drain_hosted_actor_events();
        Ok(())
    }

    fn stop_session_init_actor(&mut self, session: SurfaceSessionId) -> Result<()> {
        let retirement_count = self.realm_inits.get(&session).map_or(0, |init| {
            init.children
                .values()
                .filter(|actor_id| self.wasm_io_actors.contains_key(*actor_id))
                .count()
                + usize::from(self.wasm_io_actors.contains_key(&init.actor_id))
        });
        ensure!(
            self.backend_lifecycle_admission
                .active()
                .saturating_add(retirement_count)
                <= BACKEND_LIFECYCLE_CAPACITY,
            "backend actor lifecycle lane has no shutdown capacity"
        );
        let starting = std::mem::take(&mut self.starting_realm_children);
        for key in starting {
            if key.0 == session {
                self.stop_requested_while_starting.insert(key);
            } else {
                self.starting_realm_children.insert(key);
            }
        }
        let stale_requests = self
            .pending_realm_artifacts
            .iter()
            .filter_map(|(request, pending)| (pending.session == session).then_some(*request))
            .collect::<BTreeSet<_>>();
        self.pending_realm_artifacts
            .retain(|request, _pending| !stale_requests.contains(request));
        self.queued_realm_artifacts
            .retain(|(request, _path)| !stale_requests.contains(request));
        let Some(init) = self.realm_inits.remove(&session) else {
            return Ok(());
        };
        let mut first_error = None;
        for actor_id in init.children.values() {
            let lifecycle = self.configured_plugin_lifecycles.remove(actor_id);
            let Some(actor) = self.wasm_io_actors.remove(actor_id) else {
                continue;
            };
            if let Err(error) = self.spawn_actor_retirement(
                actor_id.clone(),
                actor,
                lifecycle,
                brood_wasm::PluginStopReason::Shutdown,
                None,
            ) && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        let lifecycle = self.configured_plugin_lifecycles.remove(&init.actor_id);
        if let Some(actor) = self.wasm_io_actors.remove(&init.actor_id)
            && let Err(error) = self.spawn_actor_retirement(
                init.actor_id.clone(),
                actor,
                lifecycle,
                brood_wasm::PluginStopReason::Shutdown,
                None,
            )
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Service lifecycle requests and failure feedback for the session init
    /// actor. This is safe to call on every compositor turn.
    pub(crate) fn service_realm_init_actor(&mut self) {
        self.service_package_declarations();
        self.drain_backend_lifecycle_completions();
        self.flush_presentation_actor_messages();
        if let Some(fast_path) = self.dmabuf.fast_path.as_mut() {
            flush_managed_resize_statuses(fast_path);
        }
        self.drain_hosted_actor_events();
        let completed = self
            .realm_inits
            .iter()
            .flat_map(|(session, init)| {
                init.apps.iter().filter_map(|(id, token)| {
                    let event = match self.hosted_child_status(*token) {
                        HostedChildStatus::Exited { success: true, .. } => {
                            brood_wasm::ComponentAppEvent::Stopped { id: id.clone() }
                        }
                        HostedChildStatus::Exited {
                            success: false,
                            detail,
                        } => brood_wasm::ComponentAppEvent::Failed {
                            id: id.clone(),
                            message: detail,
                        },
                        HostedChildStatus::Launching
                        | HostedChildStatus::Running
                        | HostedChildStatus::Unknown => return None,
                    };
                    Some((*session, id.clone(), event))
                })
            })
            .collect::<Vec<_>>();
        for (session, id, event) in completed {
            if let Some(init) = self.realm_inits.get_mut(&session) {
                init.apps.remove(&id);
            }
            self.send_realm_init_app_event(session, event);
        }
    }

    fn package_resource_samples(
        &self,
    ) -> Vec<(String, u64, std::collections::BTreeMap<String, u64>)> {
        self.package_host
            .records()
            .into_iter()
            .filter_map(|record| {
                let actor = self.wasm_io_actors.get(&record.actor)?;
                let stats = actor.runtime_io_stats();
                Some((
                    record.actor,
                    actor.generation(),
                    std::collections::BTreeMap::from([
                        ("resources".into(), stats.resources as u64),
                        ("streams".into(), stats.streams as u64),
                        ("children".into(), stats.children as u64),
                        ("timers".into(), stats.timers as u64),
                    ]),
                ))
            })
            .collect()
    }

    fn service_package_declarations(&mut self) {
        self.flush_package_services();
        if !self.package_host.poll() {
            return;
        }
        self.package_host
            .observe_resources(self.package_resource_samples());
        self.package_host.sync_services();
        {
            let mut broker = self.package_host.services.borrow_mut();
            let route = self
                .dmabuf
                .fast_path
                .as_ref()
                .map(|path| format!("presentation:{}", path.generation));
            broker.retain_routes(&route.iter().cloned().collect::<Vec<_>>());
            if let Some(route) = route {
                broker.connect(&route, Some(self.sessions.active().raw().to_string()));
            }
            if let Err(error) = broker.publish().and_then(|()| broker.reconcile()) {
                warn!(%error, "package services pending");
            }
        }
        self.flush_package_services();
        if let Ok(body) = self.package_host.update(
            brood_cli::package_runtime::contract::Realm::Daemon,
            "daemon",
        ) && let Err(error) =
            self.send_backend_actor_message(brood_cli::package_runtime::contract::HOST_SENDER, body)
            && self.backend_init.is_some()
        {
            warn!(%error, "daemon package declaration pending");
        }
        for (session, init) in &self.realm_inits {
            let Some(actor) = self.wasm_io_actors.get(&init.actor_id) else {
                continue;
            };
            let body = self.package_host.update(
                brood_cli::package_runtime::contract::Realm::SurfaceSession,
                &session.raw().to_string(),
            );
            match body.and_then(|body| {
                actor.try_send_message(
                    brood_cli::package_runtime::contract::HOST_SENDER.into(),
                    body,
                )
            }) {
                Ok(()) => {}
                Err(error) => {
                    warn!(session = session.raw(), error = %error, "package declaration delivery pending");
                }
            }
        }
    }

    pub(crate) fn accept_package_packet(&mut self, payload: &[u8]) -> anyhow::Result<()> {
        let generation = self
            .dmabuf
            .fast_path
            .as_ref()
            .context("presentation disconnected")?
            .generation;
        let route = format!("presentation:{generation}");
        self.package_host
            .services
            .borrow_mut()
            .connect(&route, Some(self.sessions.active().raw().to_string()));
        self.package_host.service_packet(&route, payload)?;
        self.flush_package_services();
        Ok(())
    }

    fn flush_package_services(&mut self) {
        use brood_cli::{
            package_runtime::contract::services::{GUEST_SENDER, HOST_ENDPOINT},
            package_services::Delivery,
        };
        let mut blocked = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let delivery = self
                .package_host
                .services
                .borrow_mut()
                .pop_unblocked(&blocked);
            let Some(delivery) = delivery else {
                break;
            };
            let result =
                match &delivery {
                    Delivery::Guest {
                        actor,
                        generation,
                        event,
                    } => self
                        .wasm_io_actors
                        .get(actor)
                        .filter(|actor| actor.generation() == *generation)
                        .map_or(Ok(()), |actor| {
                            serde_json::to_vec(event)
                                .map_err(anyhow::Error::from)
                                .and_then(|body| actor.try_send_message(GUEST_SENDER.into(), body))
                        }),
                    Delivery::Remote { route, packet } => {
                        if self.dmabuf.fast_path.as_ref().is_some_and(|path| {
                            *route == format!("presentation:{}", path.generation)
                        }) {
                            packet
                                .encode()
                                .map_err(anyhow::Error::msg)
                                .and_then(|body| {
                                    self.send_presentation_actor_message(
                                        BackendActorRealm::Daemon,
                                        HOST_ENDPOINT,
                                        HOST_ENDPOINT,
                                        &body,
                                    )
                                })
                        } else {
                            Ok(())
                        }
                    }
                };
            if result.is_err() {
                blocked.insert(delivery.lane());
                self.package_host.services.borrow_mut().retry(delivery);
            }
        }
        self.package_host
            .services
            .borrow_mut()
            .defer_lanes(&blocked);
    }

    pub(crate) fn package_query(
        &self,
        bytes: &[u8],
    ) -> brood_cli::package_runtime::contract::introspection::Response {
        let query: serde_json::Value = serde_json::from_slice(bytes).unwrap_or_default();
        let mut snapshot = self.package_instances(0, usize::MAX);
        if let Some(snapshot) = snapshot.as_object_mut() {
            snapshot.insert(
                "catalogue".into(),
                self.package_host.query_catalogue(&query),
            );
        }
        brood_cli::introspection::snapshot_query(
            self.brood_dbus_destination
                .as_deref()
                .unwrap_or("broodd:local"),
            bytes,
            &snapshot,
        )
    }

    pub(crate) fn package_instances(&self, offset: usize, limit: usize) -> serde_json::Value {
        let all_records = self.package_host.records();
        let total = all_records.len();
        let mut records: Vec<_> = all_records.into_iter().skip(offset).take(limit).collect();
        for record in &mut records {
            if let Some(actor) = self.wasm_io_actors.get(&record.actor) {
                record.generation = Some(actor.generation());
                if actor.mailbox_is_closed() {
                    record.lifecycle = "failed".into();
                }
                let resources = actor.runtime_io_stats();
                record.resources_observed_at_unix_ms = Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |time| {
                            u64::try_from(time.as_millis()).unwrap_or(u64::MAX)
                        }),
                );
                record
                    .resources
                    .insert("resources".into(), resources.resources as u64);
                record
                    .resources
                    .insert("streams".into(), resources.streams as u64);
                record
                    .resources
                    .insert("children".into(), resources.children as u64);
                record
                    .resources
                    .insert("timers".into(), resources.timers as u64);
            } else if record.lifecycle == "running" {
                record.lifecycle = "unavailable".into();
                record
                    .recent_failures
                    .push("the host no longer has this actor generation".into());
            }
        }
        for record in &mut records {
            record.assess_resources();
        }
        serde_json::json!({"schema_version": 1, "endpoint": self.brood_dbus_destination,
            "observed_at_unix_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |duration| duration.as_millis()),
            "catalogue": self.package_host.describe(), "instances": records,
            "total": total, "next_offset": (offset.saturating_add(limit) < total).then_some(offset.saturating_add(limit)),
            "services": self.package_host.services.borrow().describe()})
    }

    fn handle_realm_init_start(
        &mut self,
        session: SurfaceSessionId,
        id: String,
        guest_path: &str,
        capabilities: &[String],
    ) {
        let result = self.start_realm_init_child(session, &id, guest_path, capabilities);
        match result {
            Ok(()) => {}
            Err(error) if error.downcast_ref::<MissingRealmArtifact>().is_some() => {
                let request = self.next_realm_artifact_request;
                self.next_realm_artifact_request =
                    self.next_realm_artifact_request.saturating_add(1);
                self.pending_realm_artifacts.insert(
                    request,
                    PendingRealmArtifact {
                        session,
                        id,
                        path: guest_path.to_string(),
                        capabilities: capabilities.to_vec(),
                    },
                );
                self.queued_realm_artifacts
                    .push_back((request, guest_path.to_string()));
            }
            Err(error) => {
                self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
                warn!(actor = %id, path = %guest_path, error = %error, "realm init failed to start child actor");
                self.send_realm_init_child_event(
                    session,
                    brood_wasm::ComponentChildEvent::Failed {
                        id,
                        message: format!("{error:#}"),
                    },
                );
            }
        }
    }

    fn start_realm_init_child(
        &mut self,
        session: SurfaceSessionId,
        id: &str,
        guest_path: &str,
        capabilities: &[String],
    ) -> Result<()> {
        ensure!(
            !id.trim().is_empty(),
            "init child actor id must not be empty"
        );
        ensure!(
            id != SESSION_INIT_ACTOR_PREFIX,
            "init actor cannot replace itself"
        );
        let actor_id = session_init_child_actor_id(session, id);
        ensure!(
            !self.wasm_io_actors.contains_key(&actor_id)
                && !self
                    .starting_realm_children
                    .contains(&(session, id.to_string())),
            "init child actor is already running or starting"
        );
        let path = if guest_path.starts_with("package://") {
            let (path, declaration) = self.package_host.resolve(
                brood_cli::package_runtime::contract::Realm::SurfaceSession,
                &session.raw().to_string(),
                id,
                guest_path,
                capabilities,
            )?;
            self.package_host.starting(
                brood_cli::package_runtime::contract::Realm::SurfaceSession,
                &session.raw().to_string(),
                &actor_id,
                &session_init_actor_id(session),
                declaration,
            );
            path
        } else {
            let relative = Path::new(guest_path);
            ensure!(
                !guest_path.trim().is_empty()
                    && !relative.is_absolute()
                    && !relative.components().any(|component| matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )),
                "init child actor path must stay inside the init package root"
            );
            let root = self
                .realm_inits
                .get(&session)
                .context("session init actor is not running")?
                .root
                .clone();
            let canonical_root = root
                .canonicalize()
                .with_context(|| format!("resolve init package root {}", root.display()))?;
            let unresolved = root.join(relative);
            if !unresolved.exists() {
                return Err(MissingRealmArtifact {
                    path: guest_path.to_string(),
                }
                .into());
            }
            let path = unresolved.canonicalize().with_context(|| {
                format!("resolve init child actor {}", root.join(relative).display())
            })?;
            ensure!(
                path.starts_with(&canonical_root),
                "init child actor path must stay inside the init package root"
            );
            path
        };
        ensure!(
            capabilities.iter().all(|capability| {
                matches!(
                    capability.as_str(),
                    "filesystem:workspace-read"
                        | "filesystem:workspace-write"
                        | "pty"
                        | "network:loopback"
                        | "computer-use"
                ) || capability
                    .strip_prefix("subprocess:")
                    .is_some_and(|command| !command.trim().is_empty())
            }),
            "init child requested an unknown capability"
        );
        let filesystem =
            if let Some(writable) = crate::config::workspace_filesystem_capability(capabilities) {
                let host_path = self
                    .sessions
                    .working_directory(session)
                    .map(PathBuf::from)
                    .context("surface session has no working directory")?;
                Some(HostedPluginFilesystem {
                    host_path,
                    writable,
                })
            } else {
                None
            };
        let runtime_io = crate::config::runtime_io_capabilities(capabilities)?;
        let lifecycle = ConfiguredPluginLifecycle {
            start_timeout: Duration::from_secs(2),
            stop_timeout: Duration::from_secs(2),
            data: BTreeMap::new(),
        };
        let lifecycle_request = (lifecycle, brood_wasm::PluginStartReason::Initial);
        self.starting_realm_children
            .insert((session, id.to_string()));
        let load_result = self.replace_wasm_component_actor(
            &actor_id,
            &path,
            filesystem.as_ref(),
            Some(&lifecycle_request),
            ConfiguredPluginResources::default(),
            runtime_io,
            if capabilities
                .iter()
                .any(|capability| capability == "computer-use")
            {
                NativeToolAccess::ComputerUse
            } else {
                NativeToolAccess::None
            },
            false,
            false,
            Some(session),
            true,
            HostedReplacementPurpose::RealmChild {
                session,
                id: id.to_string(),
            },
        );
        if let Err(error) = load_result {
            self.starting_realm_children
                .remove(&(session, id.to_string()));
            return Err(error);
        }
        Ok(())
    }

    fn spawn_realm_child_stop(
        &mut self,
        session: SurfaceSessionId,
        id: String,
        actor_id: String,
        mut actor: brood_wasm::HostedWasmActor,
        lifecycle: Option<ConfiguredPluginLifecycle>,
    ) -> Result<()> {
        ensure!(
            self.try_admit_backend_lifecycle_reserved(),
            "backend actor lifecycle lane is full"
        );
        let runtime = self
            .actor_runtime
            .clone()
            .context("backend actor runtime is not initialized")?;
        let completion = self.backend_lifecycle_completion_sender.clone();
        let signal = self.loop_signal.clone();
        let task = runtime.spawn(async move {
            let lifecycle_result = if let Some(lifecycle) = lifecycle {
                let generation = actor.generation();
                let event = brood_wasm::PluginLifecycleEvent::Stop(brood_wasm::PluginStopContext {
                    instance_id: actor_id,
                    generation,
                    reason: brood_wasm::PluginStopReason::Shutdown,
                    next_generation: None,
                });
                actor
                    .lifecycle(event, lifecycle.stop_timeout)
                    .await
                    .map_err(|error| format!("plugin stop lifecycle failed: {error:#}"))
            } else {
                Ok(())
            };
            let shutdown_result = actor
                .shutdown()
                .await
                .map_err(|error| format!("plugin shutdown failed: {error:#}"));
            let result = lifecycle_result.and(shutdown_result);
            signal.wakeup();
            let _sent = completion
                .send(BackendLifecycleCompletion::RealmChildStopped {
                    session,
                    id,
                    result,
                })
                .await;
            signal.wakeup();
        });
        self.backend_lifecycle_tasks.push(task);
        self.stats
            .backend_lifecycle
            .record_admission(self.backend_lifecycle_admission.active());
        Ok(())
    }

    fn spawn_actor_retirement(
        &mut self,
        actor_id: String,
        mut actor: brood_wasm::HostedWasmActor,
        lifecycle: Option<ConfiguredPluginLifecycle>,
        reason: brood_wasm::PluginStopReason,
        next_generation: Option<u64>,
    ) -> Result<()> {
        ensure!(
            self.try_admit_backend_lifecycle_reserved(),
            "backend actor lifecycle lane is full"
        );
        let runtime = self
            .actor_runtime
            .clone()
            .context("backend actor runtime is not initialized")?;
        let completion = self.backend_lifecycle_completion_sender.clone();
        let signal = self.loop_signal.clone();
        let task_actor_id = actor_id.clone();
        let task = runtime.spawn(async move {
            let lifecycle_result = if let Some(lifecycle) = lifecycle {
                let generation = actor.generation();
                let event = brood_wasm::PluginLifecycleEvent::Stop(brood_wasm::PluginStopContext {
                    instance_id: task_actor_id,
                    generation,
                    reason,
                    next_generation,
                });
                actor
                    .lifecycle(event, lifecycle.stop_timeout)
                    .await
                    .map_err(|error| format!("plugin stop lifecycle failed: {error:#}"))
            } else {
                Ok(())
            };
            let shutdown_result = actor
                .shutdown()
                .await
                .map_err(|error| format!("plugin shutdown failed: {error:#}"));
            let result = lifecycle_result.and(shutdown_result);
            signal.wakeup();
            let _sent = completion
                .send(BackendLifecycleCompletion::ActorRetired { actor_id, result })
                .await;
            signal.wakeup();
        });
        self.backend_lifecycle_tasks.push(task);
        self.stats
            .backend_lifecycle
            .record_admission(self.backend_lifecycle_admission.active());
        Ok(())
    }

    fn drain_backend_lifecycle_completions(&mut self) {
        self.backend_lifecycle_tasks
            .retain(|task| !task.is_finished());
        for _ in 0..BACKEND_LIFECYCLE_CAPACITY {
            let Ok(completion) = self.backend_lifecycle_completions.try_recv() else {
                break;
            };
            self.backend_lifecycle_admission.complete();
            self.stats
                .backend_lifecycle
                .record_completion(self.backend_lifecycle_admission.active());
            match completion {
                BackendLifecycleCompletion::HostedReplacementReady {
                    request,
                    actor_id,
                    path,
                    purpose,
                    lifecycle,
                    result,
                } => {
                    let current =
                        self.pending_hosted_replacements.get(&actor_id).copied() == Some(request);
                    if !current {
                        if let Ok(actor) = result {
                            let generation = actor.generation();
                            if let Err(error) = self.spawn_actor_retirement(
                                actor_id.clone(),
                                *actor,
                                lifecycle,
                                brood_wasm::PluginStopReason::Replacement,
                                None,
                            ) {
                                warn!(actor = %actor_id, error = %error, "failed to retire stale hosted actor candidate");
                            }
                            debug!(actor = %actor_id, generation, "discarded stale hosted actor candidate");
                        }
                        continue;
                    }
                    self.pending_hosted_replacements.remove(&actor_id);
                    let startup_messages = self.pending_hosted_startup_messages.take(&actor_id);
                    let actor = match result {
                        Ok(actor) => *actor,
                        Err(message) => {
                            self.plugin_actor_failures =
                                self.plugin_actor_failures.saturating_add(1);
                            self.plugin_actor_last_status =
                                Some(format!("{actor_id}:replacement_failed"));
                            match purpose {
                                HostedReplacementPurpose::DaemonChild { id } => {
                                    self.starting_daemon_children.remove(&id);
                                    self.stop_daemon_while_starting.remove(&id);
                                    self.send_daemon_child_event(
                                        brood_wasm::ComponentChildEvent::Failed { id, message },
                                    );
                                }
                                HostedReplacementPurpose::RealmChild { session, id } => {
                                    self.starting_realm_children.remove(&(session, id.clone()));
                                    self.stop_requested_while_starting
                                        .remove(&(session, id.clone()));
                                    self.send_realm_init_child_event(
                                        session,
                                        brood_wasm::ComponentChildEvent::Failed { id, message },
                                    );
                                }
                                HostedReplacementPurpose::RealmRoot { session } => {
                                    self.realm_inits.remove(&session);
                                    warn!(actor = %actor_id, surface_session = session.raw(), path = %path.display(), error = %message, "realm root actor start failed");
                                }
                                HostedReplacementPurpose::Generic => {
                                    warn!(actor = %actor_id, path = %path.display(), error = %message, "hosted actor replacement failed");
                                }
                            }
                            continue;
                        }
                    };
                    let generation = actor.generation();
                    if let HostedReplacementPurpose::RealmChild { session, id } = &purpose {
                        self.starting_realm_children.remove(&(*session, id.clone()));
                        let cancelled = self
                            .stop_requested_while_starting
                            .remove(&(*session, id.clone()));
                        let realm_exists = self.realm_inits.contains_key(session);
                        if cancelled || !realm_exists {
                            if let Err(error) = self.spawn_actor_retirement(
                                actor_id.clone(),
                                actor,
                                lifecycle,
                                brood_wasm::PluginStopReason::Shutdown,
                                None,
                            ) {
                                warn!(actor = %actor_id, error = %error, "failed to retire cancelled realm child candidate");
                            }
                            continue;
                        }
                    }
                    if let HostedReplacementPurpose::RealmRoot { session } = &purpose
                        && !self.realm_inits.contains_key(session)
                    {
                        if let Err(error) = self.spawn_actor_retirement(
                            actor_id.clone(),
                            actor,
                            lifecycle,
                            brood_wasm::PluginStopReason::Shutdown,
                            None,
                        ) {
                            warn!(actor = %actor_id, error = %error, "failed to retire cancelled realm root candidate");
                        }
                        continue;
                    }
                    let old_lifecycle = self.configured_plugin_lifecycles.remove(&actor_id);
                    let old_actor = self.wasm_io_actors.insert(actor_id.clone(), actor);
                    if let Some(config) = lifecycle.clone() {
                        self.configured_plugin_lifecycles
                            .insert(actor_id.clone(), config);
                    }
                    if let Some(old_actor) = old_actor
                        && let Err(error) = self.spawn_actor_retirement(
                            actor_id.clone(),
                            old_actor,
                            old_lifecycle,
                            brood_wasm::PluginStopReason::Replacement,
                            Some(generation),
                        )
                    {
                        warn!(actor = %actor_id, error = %error, "failed to retire replaced hosted actor");
                    }
                    match purpose {
                        HostedReplacementPurpose::DaemonChild { id } => {
                            self.commit_daemon_child(id, &actor_id, generation);
                        }
                        HostedReplacementPurpose::Generic => {
                            self.plugin_actor_last_status =
                                Some(format!("{actor_id}:replacement_started"));
                        }
                        HostedReplacementPurpose::RealmRoot { .. } => {
                            self.plugin_actor_last_status =
                                Some(format!("{actor_id}:realm_started"));
                        }
                        HostedReplacementPurpose::RealmChild { session, id } => {
                            if let Some(init) = self.realm_inits.get_mut(&session) {
                                init.children.insert(id.clone(), actor_id.clone());
                            }
                            self.send_realm_init_child_event(
                                session,
                                brood_wasm::ComponentChildEvent::Started { id, generation },
                            );
                        }
                    }
                    for (sender, body) in startup_messages {
                        if let Err(error) =
                            self.try_route_wasm_actor_message(&actor_id, sender, body)
                        {
                            warn!(actor = %actor_id, error = %error, "failed to deliver queued hosted actor startup message");
                        }
                    }
                    info!(actor = %actor_id, generation, path = %path.display(), "committed hosted actor replacement");
                }
                BackendLifecycleCompletion::LegacyReplacementReady {
                    request,
                    path,
                    generation,
                    result,
                } => {
                    if self.pending_legacy_replacement != Some(request) {
                        warn!(request, path = %path.display(), "discarded stale legacy replacement completion");
                        if let Some(generation) = generation {
                            let runtime = self.actor_runtime.clone();
                            if let Some(runtime) = runtime {
                                self.backend_lifecycle_tasks.push(runtime.spawn(async move {
                                    generation.cancel_and_join().await;
                                }));
                            }
                        }
                        continue;
                    }
                    self.pending_legacy_replacement = None;
                    self.pending_legacy_actor_names.clear();
                    if let Some(generation) = generation {
                        self.legacy_wasm_actor_generation = Some(generation);
                    }
                    match result {
                        Ok(()) => {
                            self.plugin_actor_last_status =
                                Some("legacy:replacement_started".to_string());
                            info!(path = %path.display(), "committed legacy WASM actor replacement");
                        }
                        Err(message) => {
                            self.plugin_actor_failures =
                                self.plugin_actor_failures.saturating_add(1);
                            self.plugin_actor_last_status =
                                Some("legacy:replacement_failed".to_string());
                            warn!(path = %path.display(), error = %message, "legacy WASM actor replacement failed");
                        }
                    }
                }
                BackendLifecycleCompletion::RealmChildStopped {
                    session,
                    id,
                    result,
                } => {
                    let event = match result {
                        Ok(()) => brood_wasm::ComponentChildEvent::Stopped { id },
                        Err(message) => brood_wasm::ComponentChildEvent::Failed { id, message },
                    };
                    self.send_realm_init_child_event(session, event);
                }
                BackendLifecycleCompletion::ActorRetired { actor_id, result } => {
                    if let Some(id) = self.stopping_daemon_children.remove(&actor_id) {
                        self.send_daemon_child_event(match result {
                            Ok(()) => brood_wasm::ComponentChildEvent::Stopped { id },
                            Err(message) => brood_wasm::ComponentChildEvent::Failed { id, message },
                        });
                    } else if let Err(message) = result {
                        warn!(actor = %actor_id, error = %message, "backend actor retirement failed");
                    }
                }
            }
        }
        if !self.backend_lifecycle_completions.is_empty() {
            self.loop_signal.wakeup();
        }
    }

    fn admit_backend_lifecycle_start_request(&mut self) -> Result<u64> {
        if let Some(request) = try_admit_lifecycle_request_id(
            &mut self.next_hosted_replacement_request,
            &mut self.backend_lifecycle_admission,
        )
        .context("allocate backend actor lifecycle request ID")?
        {
            return Ok(request);
        }
        self.stats.backend_lifecycle.deferred_turns = self
            .stats
            .backend_lifecycle
            .deferred_turns
            .saturating_add(1);
        self.stats.backend_lifecycle.dropped =
            self.stats.backend_lifecycle.dropped.saturating_add(1);
        anyhow::bail!("backend actor lifecycle start lane is full")
    }

    const fn try_admit_backend_lifecycle_reserved(&mut self) -> bool {
        if self.backend_lifecycle_admission.try_admit_reserved() {
            true
        } else {
            self.stats.backend_lifecycle.deferred_turns = self
                .stats
                .backend_lifecycle
                .deferred_turns
                .saturating_add(1);
            self.stats.backend_lifecycle.dropped =
                self.stats.backend_lifecycle.dropped.saturating_add(1);
            false
        }
    }

    /// Join the bounded lifecycle lane after calloop has exited, then commit
    /// its queued completions. Completion handling can schedule retirement,
    /// so shutdown repeats until no generation-owned lifecycle task remains.
    pub(crate) fn finish_backend_lifecycle_tasks(&mut self) -> Result<()> {
        ensure!(
            !self.event_loop_active,
            "backend lifecycle joins are shutdown-only"
        );
        let runtime = self
            .actor_runtime
            .clone()
            .context("backend actor runtime is not initialized")?;
        let mut first_error = None;
        while !self.backend_lifecycle_tasks.is_empty() {
            let tasks = std::mem::take(&mut self.backend_lifecycle_tasks);
            runtime.block_on(async {
                for task in tasks {
                    if let Err(error) = task.await
                        && first_error.is_none()
                    {
                        first_error = Some(anyhow::anyhow!(
                            "backend lifecycle task failed to join: {error}"
                        ));
                    }
                }
            });
            self.drain_backend_lifecycle_completions();
        }
        first_error.map_or(Ok(()), Err)
    }

    fn handle_realm_init_stop(&mut self, session: SurfaceSessionId, id: String) {
        if self
            .starting_realm_children
            .contains(&(session, id.clone()))
        {
            self.stop_requested_while_starting.insert((session, id));
            return;
        }
        let owned = self
            .realm_inits
            .get(&session)
            .and_then(|init| init.children.get(&id))
            .cloned();
        let Some(actor_id) = owned else {
            if self.package_host.owns(
                brood_cli::package_runtime::contract::Realm::SurfaceSession,
                &session.raw().to_string(),
                &id,
            ) {
                self.send_realm_init_child_event(
                    session,
                    brood_wasm::ComponentChildEvent::Stopped { id },
                );
                return;
            }
            self.send_realm_init_child_event(
                session,
                brood_wasm::ComponentChildEvent::Failed {
                    id,
                    message: "init actor may stop only its own children".to_string(),
                },
            );
            return;
        };
        if self.backend_lifecycle_admission.active() >= BACKEND_LIFECYCLE_CAPACITY {
            self.stats.backend_lifecycle.deferred_turns = self
                .stats
                .backend_lifecycle
                .deferred_turns
                .saturating_add(1);
            self.send_realm_init_child_event(
                session,
                brood_wasm::ComponentChildEvent::Failed {
                    id,
                    message: "backend actor lifecycle lane is full".to_string(),
                },
            );
            return;
        }
        let actor = self.wasm_io_actors.remove(&actor_id);
        let lifecycle = self.configured_plugin_lifecycles.remove(&actor_id);
        if let Some(init) = self.realm_inits.get_mut(&session) {
            init.children.remove(&id);
        }
        let Some(actor) = actor else {
            self.send_realm_init_child_event(
                session,
                brood_wasm::ComponentChildEvent::Failed {
                    id,
                    message: "init child actor is unavailable".to_string(),
                },
            );
            return;
        };
        if let Err(error) =
            self.spawn_realm_child_stop(session, id.clone(), actor_id, actor, lifecycle)
        {
            self.send_realm_init_child_event(
                session,
                brood_wasm::ComponentChildEvent::Failed {
                    id,
                    message: format!("{error:#}"),
                },
            );
        }
    }

    fn handle_realm_init_app_start(
        &mut self,
        session: SurfaceSessionId,
        id: String,
        argv: &[String],
    ) {
        let result = (|| {
            ensure!(!id.trim().is_empty(), "init app id must not be empty");
            ensure!(!argv.is_empty(), "init app argv must not be empty");
            ensure!(
                self.realm_inits
                    .get(&session)
                    .is_some_and(|init| !init.apps.contains_key(&id)),
                "init app id is already running"
            );
            self.spawn_hosted_app_argv_in_session(argv, session)
        })();
        match result {
            Ok(token) => {
                if let Some(init) = self.realm_inits.get_mut(&session) {
                    init.apps.insert(id.clone(), token);
                }
                self.send_realm_init_app_event(
                    session,
                    brood_wasm::ComponentAppEvent::Started {
                        id,
                        pane: token.raw(),
                    },
                );
            }
            Err(error) => self.send_realm_init_app_event(
                session,
                brood_wasm::ComponentAppEvent::Failed {
                    id,
                    message: format!("{error:#}"),
                },
            ),
        }
    }

    fn realm_init_root_session(&self, actor_id: &str) -> Option<SurfaceSessionId> {
        self.realm_inits
            .iter()
            .find_map(|(session, init)| (init.actor_id == actor_id).then_some(*session))
    }

    fn realm_init_child_owner(&self, actor_id: &str) -> Option<(SurfaceSessionId, String)> {
        self.realm_inits.iter().find_map(|(session, init)| {
            init.children
                .iter()
                .find_map(|(id, hosted_id)| (hosted_id == actor_id).then(|| (*session, id.clone())))
        })
    }

    fn send_realm_init_child_event(
        &self,
        session: SurfaceSessionId,
        event: brood_wasm::ComponentChildEvent,
    ) {
        let realm = brood_cli::package_runtime::contract::Realm::SurfaceSession;
        let scope = session.raw().to_string();
        match &event {
            brood_wasm::ComponentChildEvent::Started { id, generation } => {
                self.package_host.started(realm, &scope, id, *generation);
            }
            brood_wasm::ComponentChildEvent::Failed { id, message } => {
                self.package_host
                    .outcome(realm, &scope, id, "failed", Some(message));
            }
            brood_wasm::ComponentChildEvent::Stopped { id } => self
                .package_host
                .outcome(realm, &scope, id, "stopped", None),
        }
        let Some(init) = self.realm_inits.get(&session) else {
            return;
        };
        let Some(brood_wasm::HostedWasmActor::Component(actor)) =
            self.wasm_io_actors.get(&init.actor_id)
        else {
            return;
        };
        if let Err(error) = actor.try_send_child_event(event) {
            warn!(error = %error, "failed to deliver child lifecycle event to session init actor");
        }
    }

    fn send_realm_init_app_event(
        &self,
        session: SurfaceSessionId,
        event: brood_wasm::ComponentAppEvent,
    ) {
        let Some(init) = self.realm_inits.get(&session) else {
            return;
        };
        let Some(brood_wasm::HostedWasmActor::Component(actor)) =
            self.wasm_io_actors.get(&init.actor_id)
        else {
            return;
        };
        if let Err(error) = actor.try_send_app_event(event) {
            warn!(error = %error, "failed to deliver app lifecycle event to session init actor");
        }
    }

    pub(crate) fn replace_wasm_plugin(&mut self, path: &Path) -> Result<()> {
        self.replace_wasm_plugin_with_instance_id(
            None,
            false,
            path,
            None,
            None,
            ConfiguredPluginResources::default(),
            brood_wasm::RuntimeIoCapabilities::default(),
        )
    }

    pub(crate) fn replace_wasm_plugin_as(&mut self, instance_id: &str, path: &Path) -> Result<()> {
        ensure!(
            !instance_id.trim().is_empty(),
            "WASM plugin instance id must not be empty"
        );
        let lifecycle = self.configured_plugin_lifecycles.get(instance_id).cloned();
        let resources = self.configured_resources_for_instance(instance_id);
        let runtime_io = self
            .configured_plugin_runtime_io
            .get(instance_id)
            .cloned()
            .unwrap_or_default();
        self.replace_wasm_plugin_with_instance_id(
            Some(instance_id),
            false,
            path,
            None,
            lifecycle.map(|config| (config, brood_wasm::PluginStartReason::Replacement)),
            resources,
            runtime_io,
        )
    }

    fn configured_resources_for_instance(&self, instance_id: &str) -> ConfiguredPluginResources {
        self.configured_plugin_resources
            .get(instance_id)
            .copied()
            .unwrap_or_default()
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "plugin replacement ABI keeps independently configured lifecycle/resource inputs; tracked outside central-state services"
    )]
    pub(crate) fn replace_configured_wasm_plugin(
        &mut self,
        module_id: &str,
        path: &Path,
        workspace_filesystem_writable: Option<bool>,
        startup_message: Option<String>,
        lifecycle: Option<ConfiguredPluginLifecycle>,
        resources: ConfiguredPluginResources,
        runtime_io: brood_wasm::RuntimeIoCapabilities,
    ) -> Result<()> {
        ensure!(
            !module_id.trim().is_empty(),
            "configured WASM module id must not be empty"
        );
        let replacing = self.wasm_io_actors.contains_key(module_id);
        // Component and Preview 1 artifacts use the config table key as their
        // hosted actor id. Legacy core artifacts predate that namespace: the
        // key remains a config label while their declared names retain the
        // singular legacy lane.
        let filesystem = if let Some(writable) = workspace_filesystem_writable {
            let host_path = self
                .sessions
                .working_directory(self.sessions.active())
                .map(Path::to_path_buf)
                .context("active workspace has no working directory")?;
            Some(HostedPluginFilesystem {
                host_path,
                writable,
            })
        } else {
            None
        };
        let start_reason = if replacing {
            brood_wasm::PluginStartReason::Replacement
        } else {
            brood_wasm::PluginStartReason::Initial
        };
        self.replace_wasm_plugin_with_instance_id(
            Some(module_id),
            true,
            path,
            filesystem,
            lifecycle.clone().map(|config| (config, start_reason)),
            resources,
            runtime_io.clone(),
        )?;
        let hosted_or_pending = self.wasm_io_actors.contains_key(module_id)
            || self.pending_hosted_replacements.contains_key(module_id);
        if let Some(writable) = workspace_filesystem_writable
            && hosted_or_pending
        {
            self.configured_workspace_plugins.insert(
                module_id.to_string(),
                ConfiguredWorkspacePlugin {
                    path: path.to_path_buf(),
                    writable,
                    startup_message: startup_message.clone(),
                    lifecycle: lifecycle.clone(),
                    resources,
                    runtime_io: runtime_io.clone(),
                },
            );
        } else {
            self.configured_workspace_plugins.remove(module_id);
        }
        if hosted_or_pending {
            self.configured_plugin_resources
                .insert(module_id.to_string(), resources);
            self.configured_plugin_runtime_io
                .insert(module_id.to_string(), runtime_io);
            if let Some(lifecycle) = lifecycle {
                self.configured_plugin_lifecycles
                    .insert(module_id.to_string(), lifecycle);
            } else {
                self.configured_plugin_lifecycles.remove(module_id);
            }
        } else {
            self.configured_plugin_resources.remove(module_id);
            self.configured_plugin_runtime_io.remove(module_id);
            self.configured_plugin_lifecycles.remove(module_id);
        }
        if let Some(message) = startup_message {
            if self.pending_hosted_replacements.contains_key(module_id) {
                self.pending_hosted_startup_messages.push(
                    module_id,
                    "host".to_string(),
                    message.into_bytes(),
                )?;
            } else {
                self.send_plugin_actor_message(module_id, "host".to_string(), message)?;
            }
        }
        Ok(())
    }

    fn stop_configured_workspace_plugins_for_rebind(&mut self) {
        let actor_names = self
            .configured_workspace_plugins
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for actor_name in actor_names {
            let result = if self.backend_lifecycle_admission.active() >= BACKEND_LIFECYCLE_CAPACITY
            {
                Err(anyhow::anyhow!("backend actor lifecycle lane is full"))
            } else if let Some(actor) = self.wasm_io_actors.remove(&actor_name) {
                self.spawn_actor_retirement(
                    actor_name.clone(),
                    actor,
                    self.configured_plugin_lifecycles.get(&actor_name).cloned(),
                    brood_wasm::PluginStopReason::Shutdown,
                    None,
                )
            } else {
                Ok(())
            };
            if let Err(error) = result {
                self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
                self.plugin_actor_last_status =
                    Some(format!("{actor_name}:workspace_rebind_stop_failed"));
                warn!(actor = %actor_name, error = %error, "failed to stop workspace plugin before session activation");
            }
        }
    }

    fn start_configured_workspace_plugins_after_rebind(&mut self) {
        let Some(host_path) = self
            .sessions
            .working_directory(self.sessions.active())
            .map(Path::to_path_buf)
        else {
            return;
        };
        let plugins = self
            .configured_workspace_plugins
            .iter()
            .map(|(id, plugin)| (id.clone(), plugin.clone()))
            .collect::<Vec<_>>();
        for (actor_name, plugin) in plugins {
            let filesystem = HostedPluginFilesystem {
                host_path: host_path.clone(),
                writable: plugin.writable,
            };
            let result = self
                .replace_wasm_plugin_with_instance_id(
                    Some(&actor_name),
                    true,
                    &plugin.path,
                    Some(filesystem),
                    plugin
                        .lifecycle
                        .clone()
                        .map(|config| (config, brood_wasm::PluginStartReason::SessionRebind)),
                    plugin.resources,
                    plugin.runtime_io,
                )
                .and_then(|()| {
                    if let Some(message) = plugin.startup_message {
                        if self.pending_hosted_replacements.contains_key(&actor_name) {
                            self.pending_hosted_startup_messages.push(
                                &actor_name,
                                "host".to_string(),
                                message.into_bytes(),
                            )?;
                        } else {
                            self.send_plugin_actor_message(
                                &actor_name,
                                "host".to_string(),
                                message,
                            )?;
                        }
                    }
                    Ok(())
                });
            if let Err(error) = result {
                self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
                self.plugin_actor_last_status =
                    Some(format!("{actor_name}:workspace_rebind_start_failed"));
                warn!(actor = %actor_name, path = %plugin.path.display(), error = %error, "failed to start workspace plugin for active session");
            }
        }
    }

    fn start_candidate_lifecycle(
        &self,
        actor_name: &str,
        actor: &brood_wasm::HostedWasmActor,
        lifecycle: &ConfiguredPluginLifecycle,
        reason: brood_wasm::PluginStartReason,
        session: Option<SurfaceSessionId>,
    ) -> Result<()> {
        ensure!(
            !self.event_loop_active,
            "blocking plugin lifecycle start is startup-only"
        );
        let runtime = self
            .actor_runtime
            .clone()
            .context("backend actor runtime is not initialized")?;
        let context = brood_wasm::PluginStartContext {
            instance_id: actor_name.to_string(),
            generation: actor.generation(),
            reason,
            backend_id: None,
            session_id: session.map(SurfaceSessionId::raw),
            workspace_root: ".".to_string(),
            data: lifecycle
                .data
                .iter()
                .map(|(key, value)| brood_wasm::PluginLifecycleEntry {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
        };
        runtime
            .block_on(actor.lifecycle(
                brood_wasm::PluginLifecycleEvent::Start(context),
                lifecycle.start_timeout,
            ))
            .context("plugin start lifecycle failed")
    }

    fn stop_hosted_io_actor_lifecycle(
        &mut self,
        actor_name: &str,
        reason: brood_wasm::PluginStopReason,
        next_generation: Option<u64>,
    ) -> Result<()> {
        ensure!(
            !self.event_loop_active,
            "blocking plugin lifecycle stop is shutdown/startup-only"
        );
        let Some(lifecycle) = self.configured_plugin_lifecycles.get(actor_name).cloned() else {
            return Ok(());
        };
        let Some(actor) = self.wasm_io_actors.get(actor_name) else {
            return Ok(());
        };
        let runtime = self
            .actor_runtime
            .clone()
            .context("backend actor runtime is not initialized")?;
        let context = brood_wasm::PluginStopContext {
            instance_id: actor_name.to_string(),
            generation: actor.generation(),
            reason,
            next_generation,
        };
        runtime
            .block_on(actor.lifecycle(
                brood_wasm::PluginLifecycleEvent::Stop(context),
                lifecycle.stop_timeout,
            ))
            .context("plugin stop lifecycle failed")
    }

    #[allow(
        clippy::needless_pass_by_value,
        clippy::too_many_arguments,
        reason = "plugin generation construction consumes these values conditionally across component and legacy branches; tracked outside central-state services"
    )]
    fn replace_wasm_plugin_with_instance_id(
        &mut self,
        instance_id: Option<&str>,
        allow_legacy_config_label: bool,
        path: &Path,
        filesystem: Option<HostedPluginFilesystem>,
        lifecycle: Option<(ConfiguredPluginLifecycle, brood_wasm::PluginStartReason)>,
        resources: ConfiguredPluginResources,
        runtime_io: brood_wasm::RuntimeIoCapabilities,
    ) -> Result<()> {
        let artifact = brood_wasm::WasmPluginArtifact::load(path)?;
        let inventory = match artifact {
            brood_wasm::WasmPluginArtifact::Component { core_module_error } => {
                let instance_id =
                    instance_id.map_or_else(|| wasm_plugin_instance_id(path), str::to_string);
                return self
                    .replace_wasm_component_actor(
                        &instance_id,
                        path,
                        filesystem.as_ref(),
                        lifecycle.as_ref(),
                        resources,
                        runtime_io,
                        NativeToolAccess::None,
                        false,
                        false,
                        Some(self.sessions.active()),
                        false,
                        HostedReplacementPurpose::Generic,
                    )
                    .with_context(|| {
                        format!(
                            "{} is neither a legacy Brood module ({core_module_error}) nor a component actor",
                            path.display()
                        )
                });
            }
            brood_wasm::WasmPluginArtifact::Preview1 => {
                let instance_id =
                    instance_id.map_or_else(|| wasm_plugin_instance_id(path), str::to_string);
                return self.replace_wasm_preview1_actor(
                    &instance_id,
                    path,
                    filesystem.as_ref(),
                    lifecycle.as_ref(),
                    resources,
                    runtime_io,
                );
            }
            brood_wasm::WasmPluginArtifact::Legacy(artifact) => artifact.inventory,
        };
        ensure_legacy_instance_id_supported(legacy_instance_id_to_validate(
            instance_id,
            allow_legacy_config_label,
        ))?;
        self.ensure_legacy_wasm_actor_ids_available(&inventory.actors)?;

        self.start_wasm_plugin_actors(path, &inventory.actors)?;
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "component generation construction combines host capability, lifecycle, and resource policy"
    )]
    fn replace_wasm_component_actor(
        &mut self,
        actor_name: &str,
        path: &Path,
        filesystem: Option<&HostedPluginFilesystem>,
        lifecycle: Option<&(ConfiguredPluginLifecycle, brood_wasm::PluginStartReason)>,
        resources: ConfiguredPluginResources,
        runtime_io: brood_wasm::RuntimeIoCapabilities,
        native_tool_access: NativeToolAccess,
        actor_management: bool,
        app_management: bool,
        lifecycle_session: Option<SurfaceSessionId>,
        presentation_messaging: bool,
        purpose: HostedReplacementPurpose,
    ) -> Result<()> {
        let Some(runtime) = self.actor_runtime.clone() else {
            anyhow::bail!("backend actor runtime is not initialized");
        };
        // Build the candidate before disturbing the currently reachable
        // generation. The candidate cannot receive messages until it is
        // inserted below, so a load/instantiate failure leaves the old actor
        // serving normally.
        self.ensure_hosted_io_actor_id_available(actor_name)?;
        let default_root = path.parent().unwrap_or_else(|| Path::new("."));
        let host_path = filesystem.map_or_else(
            || default_root.to_path_buf(),
            |grant| grant.host_path.clone(),
        );
        let writable = filesystem.is_some_and(|grant| grant.writable);
        let working_directory = lifecycle_session
            .and_then(|session| self.sessions.working_directory(session))
            .map(PathBuf::from)
            .or_else(|| filesystem.map(|grant| grant.host_path.clone()));
        let mut environment = lifecycle_session
            .map(|session| self.session_process_environment(session))
            .unwrap_or_default();
        let mut removed_environment = BTreeSet::new();
        if native_tool_access == NativeToolAccess::ComputerUse {
            if let Some(socket) = self.brood_native_tool_socket.as_ref() {
                environment.insert(
                    OsString::from(crate::BROOD_NATIVE_TOOL_SOCKET_ENV),
                    socket.clone(),
                );
            }
            for variable in [
                BROODD_DBUS_DESTINATION_ENV,
                BROODD_DBUS_ADDRESS_ENV,
                "DBUS_SESSION_BUS_ADDRESS",
            ] {
                environment.remove(std::ffi::OsStr::new(variable));
                removed_environment.insert(OsString::from(variable));
            }
        } else {
            environment.remove(std::ffi::OsStr::new(crate::BROOD_NATIVE_TOOL_SOCKET_ENV));
            removed_environment.insert(OsString::from(crate::BROOD_NATIVE_TOOL_SOCKET_ENV));
        }
        info!(
            actor = actor_name,
            computer_use = native_tool_access == NativeToolAccess::ComputerUse,
            native_tool =
                environment.contains_key(std::ffi::OsStr::new(crate::BROOD_NATIVE_TOOL_SOCKET_ENV)),
            "configured component actor process environment"
        );
        let config = brood_wasm::ComponentActorConfig {
            // Actors without an explicit workspace grant receive the
            // narrowest useful default: their package directory read-only.
            filesystem: Some(brood_wasm::FilesystemGrant {
                host_path,
                guest_path: ".".to_string(),
                writable,
            }),
            filesystem_watcher_service: self.filesystem_watcher_service.clone(),
            max_filesystem_watches: resources.filesystem_watches,
            max_filesystem_watch_events: resources.filesystem_watch_events,
            runtime_io: brood_wasm::RuntimeIoConfig {
                capabilities: runtime_io,
                working_directory,
                environment,
                removed_environment,
                ..brood_wasm::RuntimeIoConfig::default()
            },
            io_runtime: self.async_runtime.clone(),
            actor_management,
            app_management,
            presentation_messaging,
            event_waker: Some(brood_wasm::ComponentActorEventWaker::new({
                let signal = self.loop_signal.clone();
                move || signal.wakeup()
            })),
            ..brood_wasm::ComponentActorConfig::default()
        };
        if self.event_loop_active {
            let request = self.admit_backend_lifecycle_start_request()?;
            self.pending_hosted_startup_messages.take(actor_name);
            self.pending_hosted_replacements
                .insert(actor_name.to_string(), request);
            let actor_id = actor_name.to_string();
            let actor_path = path.to_path_buf();
            let lifecycle = lifecycle.cloned();
            let completion_lifecycle = lifecycle.as_ref().map(|(config, _)| config.clone());
            let completion = self.backend_lifecycle_completion_sender.clone();
            let signal = self.loop_signal.clone();
            let task_actor_id = actor_id.clone();
            let task_path = actor_path.clone();
            let task = runtime.spawn(async move {
                let result = async {
                    let actor = brood_wasm::ComponentActor::load(&task_path, config)
                        .await
                        .map_err(|error| format!("load component actor: {error:#}"))?;
                    let mut candidate = brood_wasm::HostedWasmActor::Component(actor);
                    if let Some((lifecycle, reason)) = lifecycle {
                        let event = brood_wasm::PluginLifecycleEvent::Start(
                            brood_wasm::PluginStartContext {
                                instance_id: task_actor_id.clone(),
                                generation: candidate.generation(),
                                reason,
                                backend_id: None,
                                session_id: lifecycle_session.map(SurfaceSessionId::raw),
                                workspace_root: ".".to_string(),
                                data: lifecycle
                                    .data
                                    .iter()
                                    .map(|(key, value)| brood_wasm::PluginLifecycleEntry {
                                        key: key.clone(),
                                        value: value.clone(),
                                    })
                                    .collect(),
                            },
                        );
                        if let Err(error) =
                            candidate.lifecycle(event, lifecycle.start_timeout).await
                        {
                            let _shutdown = candidate.shutdown().await;
                            return Err(format!("plugin start lifecycle failed: {error:#}"));
                        }
                    }
                    Ok(Box::new(candidate))
                }
                .await;
                signal.wakeup();
                let _sent = completion
                    .send(BackendLifecycleCompletion::HostedReplacementReady {
                        request,
                        actor_id,
                        path: actor_path,
                        purpose,
                        lifecycle: completion_lifecycle,
                        result,
                    })
                    .await;
                signal.wakeup();
            });
            self.backend_lifecycle_tasks.push(task);
            self.stats
                .backend_lifecycle
                .record_admission(self.backend_lifecycle_admission.active());
            return Ok(());
        }
        ensure!(
            !self.event_loop_active,
            "blocking component actor load is startup-only"
        );
        let actor = runtime.block_on(brood_wasm::ComponentActor::load(path, config))?;
        let mut candidate = brood_wasm::HostedWasmActor::Component(actor);
        let next_generation = candidate.generation();
        if let Err(error) = self.stop_hosted_io_actor_lifecycle(
            actor_name,
            brood_wasm::PluginStopReason::Replacement,
            Some(next_generation),
        ) {
            warn!(actor = %actor_name, error = %error, "old plugin stop hook failed; continuing hard replacement teardown");
        }
        self.stop_hosted_io_actor(actor_name)?;
        if let Some((config, reason)) = lifecycle
            && let Err(error) = self.start_candidate_lifecycle(
                actor_name,
                &candidate,
                config,
                *reason,
                lifecycle_session,
            )
        {
            let _ = runtime.block_on(candidate.shutdown());
            return Err(error);
        }
        self.wasm_io_actors
            .insert(actor_name.to_string(), candidate);
        match purpose {
            HostedReplacementPurpose::DaemonChild { id } => {
                let generation = self
                    .wasm_io_actors
                    .get(actor_name)
                    .context("daemon child disappeared before publication")?
                    .generation();
                if let Some((config, _)) = lifecycle {
                    self.configured_plugin_lifecycles
                        .insert(actor_name.into(), config.clone());
                }
                self.commit_daemon_child(id, actor_name, generation);
            }
            HostedReplacementPurpose::Generic => {
                self.plugin_actor_last_status = Some(format!("{actor_name}:component_started"));
            }
            HostedReplacementPurpose::RealmRoot { .. } => {
                // `start_session_init_actor` publishes the realm only after
                // this startup transaction has returned successfully.
                self.plugin_actor_last_status = Some(format!("{actor_name}:realm_started"));
            }
            HostedReplacementPurpose::RealmChild { session, id } => {
                self.starting_realm_children.remove(&(session, id.clone()));
                let generation = self
                    .wasm_io_actors
                    .get(actor_name)
                    .map(brood_wasm::HostedWasmActor::generation)
                    .context("startup realm child disappeared before publication")?;
                self.realm_inits
                    .get_mut(&session)
                    .context("startup realm child lost its init actor")?
                    .children
                    .insert(id.clone(), actor_name.to_string());
                if let Some((config, _reason)) = lifecycle {
                    self.configured_plugin_lifecycles
                        .insert(actor_name.to_string(), config.clone());
                }
                self.send_realm_init_child_event(
                    session,
                    brood_wasm::ComponentChildEvent::Started { id, generation },
                );
                self.plugin_actor_last_status = Some(format!("{actor_name}:realm_started"));
            }
        }
        info!(actor = %actor_name, path = %path.display(), "started component plugin actor");
        Ok(())
    }

    fn replace_wasm_preview1_actor(
        &mut self,
        actor_name: &str,
        path: &Path,
        filesystem: Option<&HostedPluginFilesystem>,
        lifecycle: Option<&(ConfiguredPluginLifecycle, brood_wasm::PluginStartReason)>,
        resources: ConfiguredPluginResources,
        runtime_io: brood_wasm::RuntimeIoCapabilities,
    ) -> Result<()> {
        let Some(runtime) = self.actor_runtime.clone() else {
            anyhow::bail!("backend actor runtime is not initialized");
        };
        // Instantiate an unreachable candidate first so a bad replacement
        // cannot take a healthy actor offline.
        self.ensure_hosted_io_actor_id_available(actor_name)?;
        let default_root = path.parent().unwrap_or_else(|| Path::new("."));
        let host_path = filesystem.map_or_else(
            || default_root.to_path_buf(),
            |grant| grant.host_path.clone(),
        );
        let writable = filesystem.is_some_and(|grant| grant.writable);
        let capabilities = brood_wasm::Preview1ActorCapabilities {
            filesystem: Some(brood_wasm::Preview1FilesystemGrant {
                host_path,
                guest_path: ".".to_string(),
                writable,
            }),
            runtime_io,
            io_runtime: self.async_runtime.clone(),
        };
        let worker_config = brood_wasm::Preview1WorkerConfig {
            max_filesystem_watches: resources.filesystem_watches,
            max_filesystem_watch_events: resources.filesystem_watch_events,
            ..brood_wasm::Preview1WorkerConfig::default()
        };
        let watcher_service = self.filesystem_watcher_service.clone();
        if self.event_loop_active {
            let request = self.admit_backend_lifecycle_start_request()?;
            self.pending_hosted_startup_messages.take(actor_name);
            self.pending_hosted_replacements
                .insert(actor_name.to_string(), request);
            let actor_id = actor_name.to_string();
            let actor_path = path.to_path_buf();
            let lifecycle = lifecycle.cloned();
            let completion_lifecycle = lifecycle.as_ref().map(|(config, _)| config.clone());
            let completion = self.backend_lifecycle_completion_sender.clone();
            let signal = self.loop_signal.clone();
            let task_actor_id = actor_id.clone();
            let task_path = actor_path.clone();
            let task = runtime.spawn(async move {
                let result = async {
                    let actor = brood_wasm::Preview1CompatActor::load_with_watcher_service(
                        &task_path,
                        capabilities,
                        worker_config,
                        watcher_service,
                    )
                    .await
                    .map_err(|error| format!("load Preview 1 actor: {error:#}"))?;
                    let mut candidate = brood_wasm::HostedWasmActor::Preview1(actor);
                    if let Some((lifecycle, reason)) = lifecycle {
                        let event = brood_wasm::PluginLifecycleEvent::Start(
                            brood_wasm::PluginStartContext {
                                instance_id: task_actor_id.clone(),
                                generation: candidate.generation(),
                                reason,
                                backend_id: None,
                                session_id: None,
                                workspace_root: ".".to_string(),
                                data: lifecycle
                                    .data
                                    .iter()
                                    .map(|(key, value)| brood_wasm::PluginLifecycleEntry {
                                        key: key.clone(),
                                        value: value.clone(),
                                    })
                                    .collect(),
                            },
                        );
                        if let Err(error) =
                            candidate.lifecycle(event, lifecycle.start_timeout).await
                        {
                            let _shutdown = candidate.shutdown().await;
                            return Err(format!("plugin start lifecycle failed: {error:#}"));
                        }
                    }
                    Ok(Box::new(candidate))
                }
                .await;
                signal.wakeup();
                let _sent = completion
                    .send(BackendLifecycleCompletion::HostedReplacementReady {
                        request,
                        actor_id,
                        path: actor_path,
                        purpose: HostedReplacementPurpose::Generic,
                        lifecycle: completion_lifecycle,
                        result,
                    })
                    .await;
                signal.wakeup();
            });
            self.backend_lifecycle_tasks.push(task);
            self.stats
                .backend_lifecycle
                .record_admission(self.backend_lifecycle_admission.active());
            return Ok(());
        }
        ensure!(
            !self.event_loop_active,
            "blocking Preview 1 actor load is startup-only"
        );
        let actor =
            runtime.block_on(brood_wasm::Preview1CompatActor::load_with_watcher_service(
                path,
                capabilities,
                worker_config,
                watcher_service,
            ))?;
        let mut candidate = brood_wasm::HostedWasmActor::Preview1(actor);
        let next_generation = candidate.generation();
        if let Err(error) = self.stop_hosted_io_actor_lifecycle(
            actor_name,
            brood_wasm::PluginStopReason::Replacement,
            Some(next_generation),
        ) {
            warn!(actor = %actor_name, error = %error, "old plugin stop hook failed; continuing hard replacement teardown");
        }
        self.stop_hosted_io_actor(actor_name)?;
        if let Some((config, reason)) = lifecycle
            && let Err(error) =
                self.start_candidate_lifecycle(actor_name, &candidate, config, *reason, None)
        {
            let _ = runtime.block_on(candidate.shutdown());
            return Err(error);
        }
        self.wasm_io_actors
            .insert(actor_name.to_string(), candidate);
        self.plugin_actor_last_status = Some(format!("{actor_name}:preview1_started"));
        info!(actor = %actor_name, path = %path.display(), "started Preview 1 compatibility plugin actor");
        Ok(())
    }

    fn ensure_hosted_io_actor_id_available(&self, actor_name: &str) -> Result<()> {
        ensure!(
            !self
                .legacy_wasm_actor_generation
                .as_ref()
                .is_some_and(|generation| generation.contains_actor(actor_name)),
            "hosted I/O actor id {actor_name:?} conflicts with a legacy WASM actor"
        );
        let conflicts_with_builtin = self.plugin_runtime.as_ref().is_some_and(|runtime| {
            runtime
                .actor_summaries()
                .iter()
                .any(|summary| summary.id == actor_name)
        });
        ensure!(
            !conflicts_with_builtin,
            "hosted I/O actor id {actor_name:?} conflicts with a built-in plugin actor"
        );
        Ok(())
    }

    fn ensure_legacy_wasm_actor_ids_available(
        &self,
        actors: &[brood_wasm::WasmActorDeclaration],
    ) -> Result<()> {
        let builtin_ids = self
            .plugin_runtime
            .as_ref()
            .map_or_else(Vec::new, PluginRuntime::actor_summaries)
            .into_iter()
            .map(|summary| summary.id)
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        for actor in actors {
            ensure!(
                seen.insert(actor.name.as_str()),
                "legacy WASM plugin declares duplicate actor id {:?}",
                actor.name
            );
            ensure!(
                !self.wasm_io_actors.contains_key(&actor.name),
                "legacy WASM actor id {:?} conflicts with a hosted I/O actor",
                actor.name
            );
            ensure!(
                !builtin_ids.iter().any(|id| id == &actor.name),
                "legacy WASM actor id {:?} conflicts with a built-in plugin actor",
                actor.name
            );
        }
        Ok(())
    }

    fn stop_hosted_io_actor(&mut self, actor_name: &str) -> Result<()> {
        ensure!(
            !self.event_loop_active,
            "blocking hosted actor join is shutdown/startup-only"
        );
        self.drain_hosted_actor_events();
        let Some(mut actor) = self.wasm_io_actors.remove(actor_name) else {
            return Ok(());
        };
        let Some(runtime) = self.actor_runtime.as_ref() else {
            anyhow::bail!("cannot join hosted I/O actor without the backend actor runtime");
        };
        runtime.block_on(async {
            let result = match &mut actor {
                brood_wasm::HostedWasmActor::Component(actor) => actor.shutdown().await,
                brood_wasm::HostedWasmActor::Preview1(actor) => {
                    actor.shutdown().await.map_err(anyhow::Error::from)
                }
            };
            if let Err(error) = result {
                // Guest failure does not prevent replacement: both backends
                // join their generation-owned work before returning it.
                warn!(actor = %actor_name, error = %error, "old hosted I/O actor stopped with an error");
            }
        });
        Ok(())
    }

    pub(crate) fn stop_all_hosted_io_actors(&mut self) -> Result<()> {
        ensure!(
            !self.event_loop_active,
            "hosted actor teardown is shutdown-only"
        );
        let actor_names = self.wasm_io_actors.keys().cloned().collect::<Vec<_>>();
        let mut first_error = None;
        for actor_name in actor_names {
            if let Err(error) = self.stop_hosted_io_actor_lifecycle(
                &actor_name,
                brood_wasm::PluginStopReason::Shutdown,
                None,
            ) {
                warn!(actor = %actor_name, error = %error, "plugin graceful shutdown hook failed; continuing hard teardown");
            }
            if let Err(error) = self.stop_hosted_io_actor(&actor_name) {
                warn!(actor = %actor_name, error = %error, "failed to join hosted I/O actor during shutdown");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub(crate) fn stop_legacy_wasm_plugin_actors(&mut self) -> Result<()> {
        ensure!(
            !self.event_loop_active,
            "legacy actor teardown is shutdown-only"
        );
        let Some(generation) = self.legacy_wasm_actor_generation.take() else {
            return Ok(());
        };
        let actor_ids = generation.actor_ids().clone();
        let Some(runtime) = self.actor_runtime.as_ref() else {
            anyhow::bail!(
                "cannot cancel and join legacy WASM actors without the backend actor runtime"
            );
        };
        let registry = Arc::clone(&self.wasm_registry);
        runtime.block_on(async move {
            generation.cancel_and_join().await;
            let mut registry = registry.lock().await;
            for (name, actor_id) in actor_ids {
                registry.unregister_actor_generation(&name, actor_id);
            }
        });
        Ok(())
    }

    fn start_wasm_plugin_actors(
        &mut self,
        path: &Path,
        actors: &[brood_wasm::WasmActorDeclaration],
    ) -> Result<()> {
        let Some(runtime) = self.actor_runtime.clone() else {
            anyhow::bail!("backend actor runtime is not initialized");
        };
        let registry = Arc::clone(&self.wasm_registry);
        if self.event_loop_active {
            ensure!(
                self.pending_legacy_replacement.is_none(),
                "legacy WASM replacement is already in progress"
            );
            let request = self.admit_backend_lifecycle_start_request()?;
            self.pending_legacy_replacement = Some(request);
            self.pending_legacy_actor_names =
                actors.iter().map(|actor| actor.name.clone()).collect();
            if let Some(old_generation) = self.legacy_wasm_actor_generation.as_ref() {
                self.pending_legacy_actor_names
                    .extend(old_generation.actor_names());
            }
            let declarations = actors.to_vec();
            let actor_path = path.to_path_buf();
            let task_path = actor_path.clone();
            let old_generation = self.legacy_wasm_actor_generation.take();
            let completion = self.backend_lifecycle_completion_sender.clone();
            let signal = self.loop_signal.clone();
            let host_events = self.plugin_host_event_sender.clone();
            let actor_runtime = runtime.clone();
            let task = runtime.spawn(async move {
                let old_names = old_generation
                    .as_ref()
                    .map_or_else(Vec::new, brood_wasm::LegacyWasmActorGeneration::actor_names);
                let prepared = async {
                    let mut candidates = Vec::with_capacity(declarations.len());
                    let mut actor_ids = BTreeMap::new();
                    let mut registrations = Vec::with_capacity(declarations.len());
                    for declaration in &declarations {
                        let (registration, mailbox) = registry
                            .lock()
                            .await
                            .prepare_actor(declaration.name.clone(), 64)?;
                        let candidate = brood_wasm::WasmActor::load_named(
                            &task_path,
                            mailbox,
                            &declaration.export_name,
                        )
                        .await
                        .with_context(|| {
                            format!("load candidate WASM actor {:?}", declaration.name)
                        })?;
                        actor_ids.insert(declaration.name.clone(), candidate.actor_id());
                        registrations.push(registration);
                        candidates.push((declaration.clone(), candidate));
                    }
                    registry
                        .lock()
                        .await
                        .validate_prepared_actors(&registrations, &old_names)?;
                    Ok::<_, anyhow::Error>((candidates, actor_ids, registrations))
                }
                .await;
                let (generation, result) = match prepared {
                    Err(error) => (old_generation, Err(format!("{error:#}"))),
                    Ok((candidates, actor_ids, registrations)) => {
                        if let Some(old_generation) = old_generation {
                            old_generation.cancel_and_join().await;
                        }
                        let activation = registry
                            .lock()
                            .await
                            .activate_prepared_actors(registrations, &old_names);
                        if let Err(error) = activation {
                            (None, Err(format!("activate legacy actors: {error:#}")))
                        } else {
                            let generation = brood_wasm::LegacyWasmActorGeneration::start(
                                &actor_runtime,
                                candidates,
                                actor_ids,
                                Arc::clone(&registry),
                                &task_path,
                                move |event| {
                                    let brood_wasm::LegacyWasmRuntimeEvent::DrainHostMailbox =
                                        event;
                                    let _sent =
                                        host_events.send(PluginHostEvent::DrainWasmHostMailbox);
                                },
                            );
                            (Some(generation), Ok(()))
                        }
                    }
                };
                signal.wakeup();
                let _sent = completion
                    .send(BackendLifecycleCompletion::LegacyReplacementReady {
                        request,
                        path: actor_path,
                        generation,
                        result,
                    })
                    .await;
                signal.wakeup();
            });
            self.backend_lifecycle_tasks.push(task);
            self.stats
                .backend_lifecycle
                .record_admission(self.backend_lifecycle_admission.active());
            return Ok(());
        }
        ensure!(
            !self.event_loop_active,
            "blocking legacy actor load is startup-only"
        );
        let mut candidates = Vec::with_capacity(actors.len());
        let mut actor_ids = BTreeMap::new();
        let mut registrations = Vec::with_capacity(actors.len());
        for actor in actors {
            let (registration, mailbox) = runtime.block_on({
                let registry = Arc::clone(&registry);
                let actor_name = actor.name.clone();
                async move { registry.lock().await.prepare_actor(actor_name, 64) }
            })?;
            let candidate = runtime
                .block_on(brood_wasm::WasmActor::load_named(
                    path,
                    mailbox,
                    &actor.export_name,
                ))
                .with_context(|| format!("load candidate WASM actor {:?}", actor.name))?;
            actor_ids.insert(actor.name.clone(), candidate.actor_id());
            registrations.push(registration);
            candidates.push((actor.clone(), candidate));
        }

        let old_names = self
            .legacy_wasm_actor_generation
            .as_ref()
            .map_or_else(Vec::new, brood_wasm::LegacyWasmActorGeneration::actor_names);
        runtime.block_on({
            let registry = Arc::clone(&registry);
            let registrations = &registrations;
            let old_names = &old_names;
            async move {
                registry
                    .lock()
                    .await
                    .validate_prepared_actors(registrations, old_names)
            }
        })?;

        // The retiring generation remains the published owner through its
        // cancellation and join. It cannot dispatch a host mutation after the
        // registry switches to the candidate generation.
        if let Some(old_generation) = self.legacy_wasm_actor_generation.take() {
            runtime.block_on(old_generation.cancel_and_join());
        }

        runtime.block_on({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .lock()
                    .await
                    .activate_prepared_actors(registrations, &old_names)
            }
        })?;

        let host_events = self.plugin_host_event_sender.clone();
        self.legacy_wasm_actor_generation = Some(brood_wasm::LegacyWasmActorGeneration::start(
            &runtime,
            candidates,
            actor_ids,
            Arc::clone(&registry),
            path,
            move |event| {
                let brood_wasm::LegacyWasmRuntimeEvent::DrainHostMailbox = event;
                let event = PluginHostEvent::DrainWasmHostMailbox;
                let _ = host_events.send(event);
            },
        ));
        Ok(())
    }

    pub fn register_extension_window(
        &mut self,
        extension_id: String,
        identity: ExtensionMetadata,
        role: &str,
        sender: String,
    ) -> Result<String> {
        self.ensure_extension_exists(&extension_id)?;
        let role = normalize_registry_value(role).context("window role must not be empty")?;
        ensure_metadata(&identity)?;
        let id = stable_registry_id("win", [&extension_id, &role], &identity);
        if let Some(record) = self
            .extension_registry
            .windows
            .iter_mut()
            .find(|record| record.id == id)
        {
            record.identity = identity;
            record.role = role;
            record.sender = sender;
        } else {
            self.extension_registry.windows.push(WindowRecord {
                id: id.clone(),
                extension_id,
                identity,
                role,
                sender,
            });
        }
        Ok(id)
    }

    pub fn replace_window_role(
        &mut self,
        window_id: &str,
        role: &str,
        options: &ExtensionMetadata,
        _sender: String,
    ) -> Result<bool> {
        let window = self.window_record(window_id)?;
        let role = normalize_registry_value(role).context("replacement role must not be empty")?;
        ensure_metadata(options)?;
        let dismiss_replaced = options
            .get("dismiss_replaced")
            .or_else(|| options.get("dismiss-replaced"))
            .and_then(|value| parse_boolish(value))
            .unwrap_or(true);

        let previous = self.extension_registry.active_window_roles.insert(
            role,
            ActiveWindowRole {
                window_id: window.id.clone(),
                extension_id: window.extension_id,
            },
        );
        let Some(previous) = previous else {
            return Ok(true);
        };
        if previous.window_id == window.id {
            return Ok(false);
        }

        self.deactivate_replaced_window(&previous.window_id, dismiss_replaced);
        Ok(true)
    }

    fn deactivate_replaced_window(&mut self, window_id: &str, dismiss: bool) {
        for grant in self.extension_registry.grants.iter_mut().filter(|grant| {
            grant
                .detail
                .get("window_id")
                .is_some_and(|id| id == window_id)
        }) {
            grant.active = false;
        }

        let Ok(window) = self.window_record(window_id) else {
            return;
        };
        let Some(surface) = self.find_surface_for_window_identity(&window.identity) else {
            return;
        };

        if self.focus.active_toplevel() == Some(&surface) {
            self.focus_toplevel_surface(None);
        }

        if dismiss
            && let Some(window) = self.window_for_any_toplevel_surface(&surface)
            && let Some(toplevel) = window.toplevel()
        {
            toplevel.send_close();
        }

        self.reconfigure_focus_stack();
    }

    pub fn request_shortcut_grant(
        &mut self,
        extension_id: &str,
        shortcut: &str,
        scope: String,
        label: String,
        sender: String,
    ) -> Result<String> {
        self.ensure_extension_exists(extension_id)?;
        let shortcut = normalize_registry_value(shortcut).context("shortcut must not be empty")?;
        ensure!(
            !brood_owned_shortcut(&shortcut),
            "shortcut conflicts with an existing Brood-owned shortcut"
        );
        let mut detail = ExtensionMetadata::new();
        detail.insert("shortcut".to_string(), shortcut);
        detail.insert("scope".to_string(), scope);
        detail.insert("label".to_string(), label);
        Ok(self.record_grant(extension_id, "shortcut", detail, sender))
    }

    pub fn request_extension_window_focus(&mut self, window_id: &str) -> Result<bool> {
        let window = self.window_record(window_id)?;
        let Some(surface) = self.find_surface_for_window_identity(&window.identity) else {
            return Ok(false);
        };
        if let Some(session) = self.session_for_toplevel_surface(&surface) {
            self.switch_to_surface_session(session);
        }
        if let Some(window) = self.window_for_any_toplevel_surface(&surface) {
            self.focus_window(&window);
        } else {
            self.focus_toplevel_surface(Some(surface));
        }
        Ok(true)
    }

    pub fn dismiss_extension_window(&mut self, window_id: &str) -> Result<bool> {
        let window = self.window_record(window_id)?;
        let Some(surface) = self.find_surface_for_window_identity(&window.identity) else {
            return Ok(false);
        };
        let Some(window) = self.window_for_any_toplevel_surface(&surface) else {
            return Ok(false);
        };
        let Some(toplevel) = window.toplevel() else {
            return Ok(false);
        };
        toplevel.send_close();
        if let Err(err) = self.display_handle.flush_clients() {
            warn!(?err, "flush clients after DBus dismiss request failed");
        }
        Ok(true)
    }

    pub fn window_for_toplevel_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.window_for_any_toplevel_surface(surface)
            .filter(|window| self.window_available_to_presentation_client(window))
    }

    fn window_for_any_toplevel_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|window| Self::window_toplevel_surface(window).as_ref() == Some(surface))
            .cloned()
    }

    fn ensure_extension_exists(&self, extension_id: &str) -> Result<()> {
        ensure!(
            self.extension_registry
                .extensions
                .iter()
                .any(|extension| extension.id == extension_id),
            "unknown extension {extension_id}"
        );
        Ok(())
    }

    fn ensure_status_publisher_exists(&self, publisher: &str) -> Result<()> {
        ensure!(
            self.extension_registry
                .status_publishers
                .iter()
                .any(|record| record.id == publisher),
            "unknown status publisher {publisher}"
        );
        Ok(())
    }

    fn window_record(&self, window_id: &str) -> Result<WindowRecord> {
        self.extension_registry
            .windows
            .iter()
            .find(|window| window.id == window_id)
            .cloned()
            .with_context(|| format!("unknown extension window {window_id}"))
    }

    fn record_grant(
        &mut self,
        extension_id: &str,
        kind: &str,
        detail: ExtensionMetadata,
        sender: String,
    ) -> String {
        let id = stable_registry_id("grant", [extension_id, kind], &detail);
        if let Some(record) = self
            .extension_registry
            .grants
            .iter_mut()
            .find(|record| record.id == id)
        {
            record.detail = detail;
            record.sender = sender;
            record.active = true;
        } else {
            self.extension_registry.grants.push(GrantRecord {
                id: id.clone(),
                extension_id: extension_id.to_string(),
                kind: kind.to_string(),
                detail,
                sender,
                active: true,
            });
        }
        id
    }

    fn assign_toplevel_surface_session(
        &mut self,
        surface: &WlSurface,
        session: SurfaceSessionId,
    ) -> bool {
        if self.sessions.assign_surface(surface, session).is_err() {
            return false;
        }
        self.reconfigure_focus_stack();
        true
    }

    fn find_surface_for_window_identity(&self, identity: &ExtensionMetadata) -> Option<WlSurface> {
        self.space.elements().find_map(|window| {
            let surface = Self::window_toplevel_surface(window)?;
            toplevel_matches_identity(self, &surface, identity).then_some(surface)
        })
    }

    pub fn set_pending_pointer_focus_surface(&mut self, surface: Option<WlSurface>) {
        self.focus.set_pending_pointer_focus(surface);
    }

    pub const fn take_pending_pointer_focus_surface(&mut self) -> Option<WlSurface> {
        self.focus.take_pending_pointer_focus()
    }

    pub fn set_pointer_hover_toplevel_surface(&mut self, surface: Option<WlSurface>) {
        self.focus.set_pointer_hover(surface);
    }

    pub fn cycle_focus(&mut self, direction: FocusDirection) -> bool {
        let windows = self
            .space
            .elements()
            .filter(|window| self.window_available_to_presentation_client(window))
            .cloned()
            .collect::<Vec<_>>();
        let window_count = windows.len();
        if window_count == 0 {
            self.focus_toplevel_surface(None);
            return false;
        }

        let active_index = self
            .focus
            .active_toplevel()
            .and_then(|active| {
                windows.iter().position(|window| {
                    Self::window_toplevel_surface(window).as_ref() == Some(active)
                })
            })
            .unwrap_or(window_count - 1);
        let next_index = match direction {
            FocusDirection::Next => (active_index + 1) % window_count,
            FocusDirection::Previous => (active_index + window_count - 1) % window_count,
        };
        let Some(next_window) = windows.get(next_index) else {
            return false;
        };
        self.focus_window(next_window);
        true
    }

    pub fn focused_window_index(&self) -> Option<usize> {
        let active = self.focus.active_toplevel()?;
        if !self.surface_can_receive_focus(active) {
            return None;
        }
        self.space
            .elements()
            .filter(|window| self.window_available_to_presentation_client(window))
            .position(|window| Self::window_toplevel_surface(window).as_ref() == Some(active))
    }

    pub fn focused_window_count(&self) -> usize {
        usize::from(self.active_window().is_some())
    }

    pub fn focused_surface_id(&self) -> Option<String> {
        let surface = self.focus.active_toplevel()?;
        if !self.surface_can_receive_focus(surface) {
            return None;
        }
        Some(crate::frame_export::sanitize_status_value(&format!(
            "{:?}",
            surface.id()
        )))
    }

    pub fn reconfigure_focus_stack(&mut self) {
        self.reconfigure_presentation_client_focus();
    }

    fn reconfigure_presentation_client_focus(&mut self) {
        let windows = self
            .space
            .elements()
            .filter(|window| self.window_available_to_presentation_client(window))
            .cloned()
            .collect::<Vec<_>>();
        let active_window = self
            .active_presentation_client_window()
            .or_else(|| windows.last().cloned());
        if let Some(window) = active_window.as_ref() {
            self.space.raise_element(window, true);
        }
        let active_surface = active_window
            .as_ref()
            .and_then(Self::window_toplevel_surface);
        self.focus.set_active_toplevel(active_surface.clone());
        self.activate_windows_without_geometry(active_surface.as_ref());
        let serial = SERIAL_COUNTER.next_serial();
        self.seat
            .get_keyboard()
            .expect("keyboard")
            .set_focus(self, active_surface, serial);
    }

    pub fn close_active_window(&mut self) -> bool {
        let Some(surface) = self.focus.active_toplevel().cloned() else {
            return false;
        };
        if !self.surface_can_receive_focus(&surface) {
            return false;
        }
        self.reset_transient_input_state();
        let closed = self.send_close_to_toplevel_surface(&surface);
        if closed && let Err(err) = self.display_handle.flush_clients() {
            warn!(?err, "flush clients after close request failed");
        }
        closed
    }

    pub fn close_pane(&mut self, pane: u64) -> bool {
        let Some(surface) = self.surface_for_pane(pane) else {
            return false;
        };
        if self.focus.active_toplevel() == Some(&surface) {
            self.reset_transient_input_state();
        }
        let closed = self.send_close_to_toplevel_surface(&surface);
        if closed && let Err(err) = self.display_handle.flush_clients() {
            warn!(?err, pane, "flush clients after pane close request failed");
        }
        closed
    }

    fn send_close_to_toplevel_surface(&self, surface: &WlSurface) -> bool {
        let Some(window) = self.window_for_any_toplevel_surface(surface) else {
            return false;
        };
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        toplevel.send_close();
        true
    }

    pub fn refresh_window_state(&mut self) {
        self.refresh_window_state_inner();
    }

    pub fn schedule_window_lifecycle_refresh(&mut self) {
        if self.window_lifecycle_refresh_pending {
            return;
        }
        self.window_lifecycle_refresh_pending = true;
        self.loop_handle.insert_idle(|state| {
            state.window_lifecycle_refresh_pending = false;
            state.refresh_window_state_from_lifecycle_event();
        });
    }

    pub fn refresh_window_state_from_lifecycle_event(&mut self) {
        let outcome = self.refresh_window_state_inner();
        self.stats.window_lifecycle_refreshes += 1;
        if outcome.focus_cleared {
            self.stats.window_lifecycle_focus_clears += 1;
        }
        if outcome.focus_repaired {
            self.stats.window_lifecycle_focus_repairs += 1;
        }
    }

    fn refresh_window_state_inner(&mut self) -> WindowRefreshOutcome {
        self.space.refresh();
        self.popups.cleanup();

        let mut outcome = WindowRefreshOutcome::default();
        let live_toplevel_surfaces = self
            .space
            .elements()
            .filter_map(|window| {
                window
                    .toplevel()
                    .map(|toplevel| toplevel.wl_surface().clone())
            })
            .collect::<Vec<_>>();
        self.fullscreen_toplevels
            .retain(|(surface, _)| live_toplevel_surfaces.iter().any(|live| live == surface));
        self.sessions.retain_live_surfaces(&live_toplevel_surfaces);
        let closed_panes = self
            .window_pane_ids
            .iter()
            .filter(|(surface, _)| !live_toplevel_surfaces.iter().any(|live| live == surface))
            .map(|(_, pane)| *pane)
            .collect::<Vec<_>>();
        for pane_id in &closed_panes {
            self.presentation_client_requested_sizes.remove(pane_id);
            for agent_id in self.unregister_agents_for_pane(*pane_id) {
                info!(
                    pane_id = *pane_id,
                    agent_id = %agent_id,
                    "unregistered agent for closed pane"
                );
            }
        }
        self.window_pane_ids
            .retain(|(surface, _)| live_toplevel_surfaces.iter().any(|live| live == surface));
        let live_toplevel_pids = live_toplevel_surfaces
            .iter()
            .filter_map(|surface| toplevel_pid(self, surface))
            .collect::<Vec<_>>();
        self.pending_child_panes.retain(|pending| {
            let Some(pid) = pending.pid else {
                return true;
            };
            let pid_string = pid.to_string();
            live_toplevel_pids.iter().any(|surface_pid| {
                surface_pid == &pid_string
                    || process_is_ancestor(surface_pid, &pid_string)
                    || process_is_ancestor(&pid_string, surface_pid)
            }) || process_parent_pid(pid).is_some()
        });
        if self
            .focus
            .pending_pointer_focus()
            .is_some_and(|surface| !live_toplevel_surfaces.iter().any(|live| live == surface))
        {
            self.focus.set_pending_pointer_focus(None);
        }
        if self
            .focus
            .pointer_hover()
            .is_some_and(|surface| !live_toplevel_surfaces.iter().any(|live| live == surface))
        {
            self.focus.set_pointer_hover(None);
        }
        let window_count = self.presentation_window_count();
        let active_surface_is_live = self.focus.active_toplevel().is_some_and(|surface| {
            self.surface_is_live_focus_target(surface, &live_toplevel_surfaces)
        });

        if self.focus.active_toplevel().is_some() && !active_surface_is_live {
            self.reset_transient_input_state();
            if let Some(next_focus) = live_toplevel_surfaces
                .iter()
                .find(|surface| self.pane_for_toplevel_surface(surface).is_some())
                .cloned()
            {
                self.focus_toplevel_surface(Some(next_focus));
                self.stats.focus_repairs += 1;
                outcome.focus_repaired = true;
            } else {
                self.focus_toplevel_surface(None);
                self.stats.focus_clears += 1;
                outcome.focus_cleared = true;
            }
        }

        let current_focus = self.seat.get_keyboard().expect("keyboard").current_focus();
        if let Some(current_focus) = current_focus {
            let focus_is_live = live_toplevel_surfaces.iter().any(|surface| {
                surface == &current_focus
                    && self.surface_is_live_focus_target(surface, &live_toplevel_surfaces)
            });
            if !focus_is_live {
                self.reset_transient_input_state();
                if let Some(next_focus) = live_toplevel_surfaces
                    .iter()
                    .find(|surface| self.pane_for_toplevel_surface(surface).is_some())
                    .cloned()
                {
                    self.focus_toplevel_surface(Some(next_focus));
                    self.stats.focus_repairs += 1;
                    outcome.focus_repaired = true;
                } else {
                    self.focus_toplevel_surface(None);
                    self.stats.focus_clears += 1;
                    outcome.focus_cleared = true;
                }
            } else if self.last_window_count > 0 && window_count == 0 {
                self.reset_transient_input_state();
                self.focus_toplevel_surface(None);
                self.stats.focus_clears += 1;
                outcome.focus_cleared = true;
            }
        }
        self.reconfigure_focus_stack();
        self.last_window_count = window_count;
        outcome
    }

    fn surface_is_live_focus_target(
        &self,
        surface: &WlSurface,
        live_toplevel_surfaces: &[WlSurface],
    ) -> bool {
        live_toplevel_surfaces.iter().any(|live| live == surface)
            && self.pane_for_toplevel_surface(surface).is_some()
    }
}

/// Dispatch the Wayland display retained inside its registered calloop source.
///
/// This is the only state-layer escape hatch for `Generic::get_mut`. Dispatch
/// may mutate Smithay protocol state, but it neither replaces the display nor
/// changes the file descriptor calloop registered for it.
fn dispatch_wayland_clients(
    display: &mut calloop::generic::NoIoDrop<Display<Brood>>,
    state: &mut Brood,
) {
    // SAFETY: the callback runs while calloop exclusively owns this source;
    // `dispatch_clients` preserves the display and its registered poll fd.
    if let Err(err) = unsafe { display.get_mut().dispatch_clients(state) } {
        state.stats.wayland_dispatch_errors += 1;
        warn!(?err, "dispatch Wayland clients failed");
    }
}

fn default_surface_session_name(session: SurfaceSessionId) -> String {
    let mut entropy = [0_u8; 8];
    if getrandom::fill(&mut entropy).is_err() {
        let fallback = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos() as u64)
            ^ u64::from(std::process::id()).rotate_left(17)
            ^ session.raw().rotate_left(37);
        entropy = fallback.to_le_bytes();
    }
    let adjective = SESSION_NAME_ADJECTIVES
        .get(usize::from(entropy[0]) % SESSION_NAME_ADJECTIVES.len())
        .copied()
        .unwrap_or("quiet");
    let noun = SESSION_NAME_NOUNS
        .get(usize::from(entropy[1]) % SESSION_NAME_NOUNS.len())
        .copied()
        .unwrap_or("workspace");
    let action = SESSION_NAME_ACTIONS
        .get(usize::from(entropy[2]) % SESSION_NAME_ACTIONS.len())
        .copied()
        .unwrap_or("working");
    let suffix = u16::from_le_bytes([entropy[3], entropy[4]]);
    format!("{adjective}-{noun}-{action}-{suffix:04x}")
}

fn session_init_actor_id(session: SurfaceSessionId) -> String {
    format!("{SESSION_INIT_ACTOR_PREFIX}.{}", session.raw())
}

fn session_init_child_actor_id(session: SurfaceSessionId, child: &str) -> String {
    format!("brood.session.{}.{}", session.raw(), child)
}

fn terminal_actor_name(ordinal: u64) -> Result<String> {
    ensure!(ordinal != 0, "terminal ordinal must be greater than zero");
    Ok(if ordinal == 1 {
        "terminal-session".to_string()
    } else {
        format!("terminal-session-{ordinal}")
    })
}

fn parse_terminal_actor_id(actor: &str) -> Option<TerminalSummary> {
    let address = actor.strip_prefix("brood.session.")?;
    let (session, actor_name) = address.split_once('.')?;
    let session = session.parse().ok()?;
    let ordinal = if actor_name == "terminal-session" {
        1
    } else {
        actor_name.strip_prefix("terminal-session-")?.parse().ok()?
    };
    (ordinal != 0).then(|| TerminalSummary {
        session,
        ordinal,
        actor: actor_name.to_string(),
    })
}

fn fallback_marker_id(index: usize) -> u64 {
    1_000_000 + u64::try_from(index).unwrap_or(u64::MAX - 1_000_000)
}

fn current_process_directory() -> PathBuf {
    let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    directory.canonicalize().unwrap_or(directory)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn load_managed_worktree_records() -> Vec<ManagedWorktreeRecord> {
    let root = brood_cli::project::managed_worktree_root();
    let Ok(project_directories) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for project_directory in project_directories.flatten() {
        let Ok(worktree_directories) = fs::read_dir(project_directory.path()) else {
            continue;
        };
        for metadata_entry in worktree_directories.flatten() {
            let metadata_path = metadata_entry.path();
            if !metadata_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".brood-worktree.json"))
            {
                continue;
            }
            let Ok(bytes) = fs::read(&metadata_path) else {
                continue;
            };
            let Ok(mut record) = serde_json::from_slice::<ManagedWorktreeRecord>(&bytes) else {
                warn!(path = %metadata_path.display(), "ignored invalid managed worktree metadata");
                continue;
            };
            let record_path = Path::new(&record.path);
            let expected_metadata = root.join(&record.metadata_path);
            if metadata_path == expected_metadata
                && record_path.starts_with(&root)
                && record_path.parent() == metadata_path.parent()
            {
                // Session IDs are daemon-generation local. A restarted backend
                // must reacquire occupation rather than preserving a stale ID.
                record.session = None;
                records.push(record);
            } else {
                warn!(path = %metadata_path.display(), "ignored mismatched managed worktree metadata");
            }
        }
    }
    records
}

fn window_surface_origin_in_compositor_space(
    window_location: Point<i32, Logical>,
    window_geometry_location: Point<i32, Logical>,
) -> Point<i32, Logical> {
    window_location - window_geometry_location
}

fn focus_stack_location(_slot: usize) -> Point<i32, Logical> {
    Point::from((0, 0))
}

fn toplevel_app_id(surface: &WlSurface) -> Option<String> {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()?
            .lock()
            .ok()?
            .app_id
            .clone()
    })
}

fn toplevel_title(surface: &WlSurface) -> Option<String> {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()?
            .lock()
            .ok()?
            .title
            .clone()
    })
}

fn toplevel_pid(state: &Brood, surface: &WlSurface) -> Option<String> {
    surface
        .client()
        .and_then(|client| client.get_credentials(&state.display_handle).ok())
        .map(|credentials| credentials.pid.to_string())
}

fn toplevel_matches_identity(
    state: &Brood,
    surface: &WlSurface,
    identity: &ExtensionMetadata,
) -> bool {
    let mut matched = false;
    if let Some(expected) = identity.get("pane_id").filter(|value| !value.is_empty()) {
        let Some(pane) = state.pane_for_toplevel_surface(surface) else {
            return false;
        };
        if expected.parse::<u64>().ok() != Some(pane) {
            return false;
        }
        matched = true;
    }
    if let Some(expected) = identity.get("app_id").filter(|value| !value.is_empty()) {
        if toplevel_app_id(surface).as_deref() != Some(expected.as_str()) {
            return false;
        }
        matched = true;
    }
    if let Some(expected) = identity.get("title").filter(|value| !value.is_empty()) {
        if toplevel_title(surface).as_deref() != Some(expected.as_str()) {
            return false;
        }
        matched = true;
    }
    if let Some(expected) = identity.get("pid").filter(|value| !value.is_empty()) {
        if toplevel_pid(state, surface).as_deref() != Some(expected.as_str()) {
            return false;
        }
        matched = true;
    }
    matched
}

fn toplevel_matches_agent_identity(
    state: &Brood,
    surface: &WlSurface,
    identity: &ExtensionMetadata,
) -> bool {
    let mut matched = false;
    if let Some(expected) = identity.get("pane_id").filter(|value| !value.is_empty()) {
        let Some(pane) = state.pane_for_toplevel_surface(surface) else {
            return false;
        };
        if expected.parse::<u64>().ok() != Some(pane) {
            return false;
        }
        matched = true;
    }
    if let Some(expected) = identity.get("app_id").filter(|value| !value.is_empty()) {
        if toplevel_app_id(surface).as_deref() != Some(expected.as_str()) {
            return false;
        }
        matched = true;
    }
    if let Some(expected) = identity.get("title").filter(|value| !value.is_empty()) {
        if toplevel_title(surface).as_deref() != Some(expected.as_str()) {
            return false;
        }
        matched = true;
    }
    if let Some(expected) = identity.get("pid").filter(|value| !value.is_empty()) {
        let Some(window_pid) = toplevel_pid(state, surface) else {
            return false;
        };
        if window_pid != *expected && !process_is_ancestor(&window_pid, expected) {
            return false;
        }
        matched = true;
    }
    matched
}

fn process_is_ancestor(parent_pid: &str, child_pid: &str) -> bool {
    let Ok(parent_pid) = parent_pid.parse::<u32>() else {
        return false;
    };
    let Ok(mut pid) = child_pid.parse::<u32>() else {
        return false;
    };
    for _ in 0..64 {
        if pid == parent_pid {
            return true;
        }
        let Some(ppid) = process_parent_pid(pid) else {
            return false;
        };
        if ppid == 0 || ppid == pid {
            return false;
        }
        pid = ppid;
    }
    false
}

fn process_parent_pid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(") ")?.1;
    let mut fields = after_name.split_whitespace();
    fields.next()?;
    fields.next()?.parse().ok()
}

fn process_tree_env_var(mut pid: u32, key: &str) -> Option<String> {
    for _ in 0..64 {
        if let Some(value) = process_env_var(pid, key) {
            return Some(value);
        }
        let Some(ppid) = process_parent_pid(pid) else {
            return None;
        };
        if ppid == 0 || ppid == pid {
            return None;
        }
        pid = ppid;
    }
    None
}

fn process_env_var(pid: u32, key: &str) -> Option<String> {
    let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
    environ.split(|byte| *byte == 0).find_map(|entry| {
        let (entry_key, entry_value) = entry.split_at_checked(key.len())?;
        if entry_key == key.as_bytes() {
            let value = entry_value.strip_prefix(b"=")?;
            Some(String::from_utf8_lossy(value).into_owned())
        } else {
            None
        }
    })
}

fn parse_boolish(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" | "visible" | "show" | "shown" => Some(true),
        "false" | "0" | "no" | "off" | "hidden" | "hide" => Some(false),
        _ => None,
    }
}

fn normalize_registry_value(value: &str) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!value.is_empty()).then_some(value)
}

fn ensure_metadata(metadata: &ExtensionMetadata) -> Result<()> {
    ensure!(
        metadata.keys().all(|key| !key.trim().is_empty()),
        "metadata keys must not be empty"
    );
    Ok(())
}

fn stable_agent_registry_id(kind: &str, metadata: &ExtensionMetadata) -> String {
    if let Some((key, value)) = ["pane_id", "pidfd", "pid"]
        .into_iter()
        .find_map(|key| metadata.get(key).map(|value| (key, value)))
        .filter(|(_, value)| !value.is_empty())
    {
        let mut identity = ExtensionMetadata::new();
        identity.insert(key.to_string(), value.clone());
        return stable_registry_id("agent", [&kind], &identity);
    }
    stable_registry_id("agent", [&kind], metadata)
}

fn merge_agent_metadata(target: &mut ExtensionMetadata, metadata: ExtensionMetadata) {
    for (key, value) in metadata {
        if key == "first_user_message"
            && target
                .get("first_user_message")
                .is_some_and(|existing| !existing.is_empty())
        {
            continue;
        }
        target.insert(key, value);
    }
}

fn stable_registry_id<'a, S>(
    prefix: &str,
    parts: impl IntoIterator<Item = &'a S>,
    metadata: &ExtensionMetadata,
) -> String
where
    S: AsRef<str> + 'a + ?Sized,
{
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        fnv1a(&mut hash, part.as_ref().as_bytes());
        fnv1a(&mut hash, b"\0");
    }
    for (key, value) in metadata {
        fnv1a(&mut hash, key.as_bytes());
        fnv1a(&mut hash, b"=");
        fnv1a(&mut hash, value.as_bytes());
        fnv1a(&mut hash, b"\0");
    }
    format!("{prefix}-{hash:016x}")
}

fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
}

fn brood_owned_shortcut(shortcut: &str) -> bool {
    let normalized = shortcut
        .split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase()
        .replace('-', "+");
    matches!(
        normalized.as_str(),
        "ctrl+b"
            | "control+b"
            | "ctrl+shift+esc"
            | "control+shift+esc"
            | "ctrl+shift+escape"
            | "control+shift+escape"
            | "w"
            | "ctrl+j"
            | "control+j"
            | "ctrl+k"
            | "control+k"
            | "ctrl+g"
            | "control+g"
            | "ctrl+enter"
            | "control+enter"
            | "ctrl+return"
            | "control+return"
            | "ctrl+1"
            | "control+1"
            | "ctrl+2"
            | "control+2"
            | "ctrl+3"
            | "control+3"
            | "ctrl+4"
            | "control+4"
            | "ctrl+5"
            | "control+5"
            | "ctrl+o"
            | "control+o"
            | "ctrl+n"
            | "control+n"
            | "ctrl+q"
            | "control+q"
            | "ctrl+shift+q"
            | "control+shift+q"
            | "ctrl+f"
            | "control+f"
            | "ctrl+l"
            | "control+l"
            | "ctrl+h"
            | "control+h"
            | "ctrl+d"
            | "control+d"
            | "ctrl+p"
            | "control+p"
            | "alt+space"
    )
}

fn parse_plugin_host_command(body: &str) -> Result<WorkspaceCommand> {
    let message: brood_wasm::HostMessage =
        serde_json::from_str(body).context("decode plugin host message")?;
    Ok(match message {
        brood_wasm::HostMessage::FocusPane { pane } => WorkspaceCommand::FocusPane(pane),
        brood_wasm::HostMessage::SpawnApp { argv } => WorkspaceCommand::SpawnApp(argv),
        brood_wasm::HostMessage::FocusOrSpawnApp { app_id, argv } => {
            WorkspaceCommand::FocusOrSpawnApp { app_id, argv }
        }
        brood_wasm::HostMessage::SwitchActiveSessionNext => {
            WorkspaceCommand::Surface(SurfaceCommand::SwitchActiveSessionNext)
        }
        brood_wasm::HostMessage::CreateAndAttachSession => {
            WorkspaceCommand::Surface(SurfaceCommand::CreateAndAttachSession)
        }
        _ => anyhow::bail!("host message is not valid in the server realm"),
    })
}

#[derive(Default)]
struct WindowRefreshOutcome {
    focus_cleared: bool,
    focus_repaired: bool,
}

#[derive(Default)]
pub struct RuntimeStats {
    pub smithay_callbacks: CallbackMetrics,
    pub smithay_driver_calls: CallbackMetrics,
    pub smithay_driver_total_micros: u128,
    pub backend_actor_events: BoundedWorkMetrics,
    pub backend_actor_messages: BoundedWorkMetrics,
    pub backend_requests: BoundedWorkMetrics,
    pub smithay_input_events: BoundedWorkMetrics,
    pub backend_lifecycle: BoundedWorkMetrics,
    pub wayland_client_insert_errors: u64,
    pub wayland_dispatch_errors: u64,
    pub xdg_configure_errors: u64,
    pub frames_rendered: u64,
    pub last_frame_at: Option<Instant>,
    pub last_frame_micros: u128,
    pub max_frame_micros: u128,
    pub last_frame_interval_micros: u128,
    pub max_frame_interval_micros: u128,
    pub client_frame_callbacks: u64,
    pub commits: u64,
    pub pending_commit_at: Option<Instant>,
    pub pending_commits: u64,
    pub commit_frame_count: u64,
    pub last_commit_batch_commits: u64,
    pub max_commit_batch_commits: u64,
    pub last_commit_to_frame_micros: Option<u128>,
    pub max_commit_to_frame_micros: u128,
    pub last_commit_to_submit_micros: Option<u128>,
    pub max_commit_to_submit_micros: u128,
    pub pending_client_frame_callback_at: Option<Instant>,
    pub pending_client_frame_callbacks: u64,
    pub client_frame_callback_commit_count: u64,
    pub last_client_frame_callback_batch_callbacks: u64,
    pub max_client_frame_callback_batch_callbacks: u64,
    pub last_client_frame_callback_to_commit_micros: Option<u128>,
    pub max_client_frame_callback_to_commit_micros: u128,
    pub client_frame_deferrals: u64,
    pub client_deferred_frames: u64,
    pub input_frame_deferrals: u64,
    pub input_deferred_frames: u64,
    pub input_events: u64,
    pub keyboard_events: u64,
    pub pointer_motion_events: u64,
    pub pointer_button_events: u64,
    pub pointer_axis_events: u64,
    pub pointer_target_hits: u64,
    pub pointer_target_misses: u64,
    pub focus_clears: u64,
    pub focus_repairs: u64,
    pub window_lifecycle_refreshes: u64,
    pub window_lifecycle_focus_clears: u64,
    pub window_lifecycle_focus_repairs: u64,
    pub pending_input_at: Option<Instant>,
    pub pending_input_events: u64,
    pub input_frame_count: u64,
    pub last_input_batch_events: u64,
    pub max_input_batch_events: u64,
    pub last_input_to_frame_micros: Option<u128>,
    pub max_input_to_frame_micros: u128,
    pub last_input_to_submit_micros: Option<u128>,
    pub max_input_to_submit_micros: u128,
    pub pending_input_commit_at: Option<Instant>,
    pub pending_input_to_commit_micros: Option<u128>,
    pub input_commit_frame_count: u64,
    pub input_without_commit_frame_count: u64,
    pub last_input_to_commit_micros: Option<u128>,
    pub max_input_to_commit_micros: u128,
    pub last_input_commit_to_submit_micros: Option<u128>,
    pub max_input_commit_to_submit_micros: u128,
}

impl RuntimeStats {
    pub fn reset_smithay_hot_path_metrics(&mut self) {
        self.smithay_callbacks = CallbackMetrics::default();
        self.smithay_driver_calls = CallbackMetrics::default();
        self.smithay_driver_total_micros = 0;
    }

    pub fn record_smithay_callback(&mut self, elapsed: Duration, driver_before_micros: u128) {
        let nested_driver_micros = self
            .smithay_driver_total_micros
            .saturating_sub(driver_before_micros);
        let exclusive_micros = elapsed.as_micros().saturating_sub(nested_driver_micros);
        self.smithay_callbacks.record(Duration::from_micros(
            u64::try_from(exclusive_micros).unwrap_or(u64::MAX),
        ));
    }

    pub fn record_smithay_driver_call(&mut self, elapsed: Duration) {
        self.smithay_driver_total_micros = self
            .smithay_driver_total_micros
            .saturating_add(elapsed.as_micros());
        self.smithay_driver_calls.record(elapsed);
    }

    pub fn record_input(&mut self, can_present_input: bool) {
        self.input_events += 1;
        if can_present_input {
            if self.pending_input_at.is_none() {
                self.pending_input_at = Some(Instant::now());
            }
            self.pending_input_events += 1;
        }
    }

    pub fn record_commit(&mut self, can_present_commit: bool) {
        self.commits += 1;
        if can_present_commit {
            let now = Instant::now();
            if self.pending_commit_at.is_none() {
                self.pending_commit_at = Some(now);
            }
            self.pending_commits += 1;

            if let Some(input_at) = self.pending_input_at
                && self.pending_input_commit_at.is_none()
            {
                self.pending_input_to_commit_micros =
                    Some(now.duration_since(input_at).as_micros());
                self.pending_input_commit_at = Some(now);
            }

            if let Some(callback_at) = self.pending_client_frame_callback_at.take() {
                let callbacks = self.pending_client_frame_callbacks;
                self.pending_client_frame_callbacks = 0;
                self.client_frame_callback_commit_count += 1;
                self.last_client_frame_callback_batch_callbacks = callbacks;
                self.max_client_frame_callback_batch_callbacks = self
                    .max_client_frame_callback_batch_callbacks
                    .max(callbacks);
                let latency = now.duration_since(callback_at).as_micros();
                self.last_client_frame_callback_to_commit_micros = Some(latency);
                self.max_client_frame_callback_to_commit_micros =
                    self.max_client_frame_callback_to_commit_micros.max(latency);
            }
        }
    }

    pub fn record_client_frame_callbacks(&mut self, callbacks: u64) {
        self.client_frame_callbacks += callbacks;
        if callbacks == 0 {
            return;
        }

        if self.pending_client_frame_callback_at.is_none() {
            self.pending_client_frame_callback_at = Some(Instant::now());
        }
        self.pending_client_frame_callbacks += callbacks;
    }

    pub fn record_frame(&mut self, frame_duration: Duration) {
        let now = Instant::now();
        self.frames_rendered += 1;
        self.last_frame_micros = frame_duration.as_micros();
        self.max_frame_micros = self.max_frame_micros.max(self.last_frame_micros);
        if let Some(last_frame_at) = self.last_frame_at.replace(now) {
            self.last_frame_interval_micros = now.duration_since(last_frame_at).as_micros();
            self.max_frame_interval_micros = self
                .max_frame_interval_micros
                .max(self.last_frame_interval_micros);
        }

        if let Some(input_at) = self.pending_input_at.take() {
            let batch_events = self.pending_input_events;
            self.pending_input_events = 0;
            self.input_frame_count += 1;
            self.last_input_batch_events = batch_events;
            self.max_input_batch_events = self.max_input_batch_events.max(batch_events);
            let latency = input_at.elapsed().as_micros();
            self.last_input_to_frame_micros = Some(latency);
            self.max_input_to_frame_micros = self.max_input_to_frame_micros.max(latency);
            self.last_input_to_submit_micros = Some(latency);
            self.max_input_to_submit_micros = self.max_input_to_submit_micros.max(latency);

            if let Some(commit_at) = self.pending_input_commit_at.take() {
                self.input_commit_frame_count += 1;
                if let Some(input_to_commit) = self.pending_input_to_commit_micros.take() {
                    self.last_input_to_commit_micros = Some(input_to_commit);
                    self.max_input_to_commit_micros =
                        self.max_input_to_commit_micros.max(input_to_commit);
                }
                let commit_to_submit = now.duration_since(commit_at).as_micros();
                self.last_input_commit_to_submit_micros = Some(commit_to_submit);
                self.max_input_commit_to_submit_micros =
                    self.max_input_commit_to_submit_micros.max(commit_to_submit);
            } else {
                self.input_without_commit_frame_count += 1;
                self.pending_input_to_commit_micros = None;
            }
        }

        if let Some(commit_at) = self.pending_commit_at.take() {
            let batch_commits = self.pending_commits;
            self.pending_commits = 0;
            self.commit_frame_count += 1;
            self.last_commit_batch_commits = batch_commits;
            self.max_commit_batch_commits = self.max_commit_batch_commits.max(batch_commits);
            let latency = commit_at.elapsed().as_micros();
            self.last_commit_to_frame_micros = Some(latency);
            self.max_commit_to_frame_micros = self.max_commit_to_frame_micros.max(latency);
            self.last_commit_to_submit_micros = Some(latency);
            self.max_commit_to_submit_micros = self.max_commit_to_submit_micros.max(latency);
        }
    }
}

#[allow(dead_code)]
pub struct PresentationFastPath {
    pub generation: u64,
    pub profile: brood_protocol::TransportProfile,
    pub input: brood_ipc::CriticalConsumer,
    pub frames: brood_ipc::CriticalProducer,
    pub catalog: brood_ipc::CriticalProducer,
    pub actor_messages: brood_ipc::ActorMessageConsumer,
    pub presentation_messages: brood_ipc::ActorMessageProducer,
    pub backend_requests: brood_ipc::BackendMessageConsumer,
    pub backend_responses: brood_ipc::BackendMessageProducer,
    pub frame_notifications: brood_ipc::EventNotifier,
    pub catalog_notifications: brood_ipc::EventNotifier,
    pub catalog_revision: u64,
    pub last_catalog_payload: Vec<u8>,
    pub presentation_message_notifications: brood_ipc::EventNotifier,
    pub backend_response_notifications: brood_ipc::EventNotifier,
    pub presentation_message_sequence: u64,
    pub deferred_presentation_messages: VecDeque<Box<brood_ipc::ActorMessageRecord>>,
    pub deferred_backend_responses: VecDeque<brood_protocol::BackendResponse>,
    /// Lossless control records waiting for capacity in the shared frame
    /// ring. Resize acknowledgements are ordered and never coalesced.
    pub deferred_managed_resize_statuses: VecDeque<brood_ipc::CriticalRecord>,
    pub resources: HashMap<WeakDmabuf, u64>,
    pub buffers: HashMap<u64, Dmabuf>,
    pub resource_app_ids: HashMap<u64, String>,
    pub window_buffers: HashMap<u64, Dmabuf>,
    pub shared_images: HashMap<u64, (String, brood_ipc::SharedImageProducer)>,
    pub window_shared_image_resources: HashMap<u64, u64>,
    pub tagged_shared_image_resources: HashMap<(u64, u64, u64), u64>,
    pub tagged_dmabuf_resources: HashMap<(u64, u64, u64), u64>,
    pub next_resource_id: u64,
    pub last_presented_sequence: Option<u64>,
    pub in_flight_sequence: Option<u64>,
    pub pending_redraw: bool,
    pub managed_resizes: ManagedResizeCorrelations,
}

const MAX_MANAGED_RESIZE_SNAPSHOTS: usize = 64;
const MAX_MANAGED_RESIZE_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_MANAGED_RESIZE_SNAPSHOT_BYTES_PER_PANE: u64 = 64 * 1024 * 1024;

fn scale_120_for_fractional(scale: f64) -> u32 {
    let scaled = scale * 120.0;
    if !scaled.is_finite() || scaled <= 0.0 {
        return 120;
    }
    scaled.round().clamp(1.0, f64::from(u32::MAX)) as u32
}

fn pixel_size_matches_scale(
    logical_size: (i32, i32),
    pixel_size: (i32, i32),
    scale_120: u32,
) -> bool {
    if scale_120 == 0 {
        return false;
    }
    [
        (logical_size.0, pixel_size.0),
        (logical_size.1, pixel_size.1),
    ]
    .into_iter()
    .all(|(logical, pixel)| {
        if logical <= 0 || pixel <= 0 {
            return false;
        }
        let Ok(pixel) = u64::try_from(pixel) else {
            return false;
        };
        let Some(numerator) =
            pixel.checked_mul(u64::from(brood_protocol::FRACTIONAL_SCALE_DENOMINATOR))
        else {
            return false;
        };
        let denominator = u64::from(scale_120);
        let Some(rounded) = numerator.checked_add(denominator / 2) else {
            return false;
        };
        u64::try_from(logical).is_ok_and(|logical| logical == rounded / denominator)
    })
}

fn managed_commit_is_suitable(
    record: &ManagedResizeCorrelation,
    current_serial: Option<Serial>,
    surface_commit: u64,
    window_size: Option<(i32, i32)>,
    new_buffer_size: Option<(i32, i32)>,
) -> bool {
    Some(record.serial) == current_serial
        && surface_commit > record.sent_after_commit
        && window_size == Some(record.logical_size)
        && new_buffer_size.is_some_and(|samples| {
            samples.0 >= record.pixel_size.0 && samples.1 >= record.pixel_size.1
        })
}

const fn managed_resize_needs_cleanup_rejection(state: &ManagedResizeCorrelationState) -> bool {
    matches!(
        state,
        ManagedResizeCorrelationState::ConfigureSent
            | ManagedResizeCorrelationState::CurrentUnsuitable
    )
}

const fn managed_resize_suppresses_ordinary_renegotiation(
    input_draining: bool,
    active_correlation: bool,
) -> bool {
    input_draining || active_correlation
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedResizeRequestAuthority {
    Tagged(u64),
    RecoverThrough(u64),
    Ordinary,
}

pub const fn managed_resize_request_authority(
    resize_epoch: Option<u64>,
    recover_through_epoch: Option<u64>,
) -> Option<ManagedResizeRequestAuthority> {
    match (resize_epoch, recover_through_epoch) {
        (Some(epoch), None) if managed_resize_epoch_is_valid(epoch) => {
            Some(ManagedResizeRequestAuthority::Tagged(epoch))
        }
        (None, Some(epoch)) if managed_resize_epoch_is_valid(epoch) => {
            Some(ManagedResizeRequestAuthority::RecoverThrough(epoch))
        }
        (None, None) => Some(ManagedResizeRequestAuthority::Ordinary),
        _ => None,
    }
}

#[derive(Debug, Eq, PartialEq)]
enum OrdinaryManagedResizeDecision {
    Configure,
    IgnoreUnscoped,
    IgnoreStaleRecovery,
    RecoverThrough(Vec<u64>),
}

pub struct ManagedResizeCorrelation {
    pub epoch: u64,
    pub serial: Serial,
    pub logical_size: (i32, i32),
    pub pixel_size: (i32, i32),
    pub sent_after_commit: u64,
    pub reserved_bytes: u64,
    pub configured_at: Instant,
    pub state: ManagedResizeCorrelationState,
}

pub enum ManagedResizeCorrelationState {
    ConfigureSent,
    CurrentUnsuitable,
    SnapshotQueued {
        surface_commit: u64,
        committed_at: Instant,
        backing: crate::frame_export::ManagedSnapshotBacking,
    },
    SceneInFlight {
        surface_commit: u64,
        scene_sequence: u64,
        transport_resource: u64,
    },
}

pub struct QueuedManagedSnapshot {
    pub epoch: u64,
    pub pixel_size: (i32, i32),
    pub surface_commit: u64,
    pub backing: crate::frame_export::ManagedSnapshotBacking,
}

#[derive(Clone, Copy)]
struct ManagedCommitObservation {
    serial: Serial,
    sample_size: (i32, i32),
}

#[derive(Default)]
pub struct ManagedResizeCorrelations {
    pub records: BTreeMap<u64, VecDeque<ManagedResizeCorrelation>>,
    pub surface_commits: BTreeMap<u64, u64>,
    pub reserved_bytes: u64,
    commit_observations: BTreeMap<u64, VecDeque<ManagedCommitObservation>>,
    ordinary_recovery_commit_floors: BTreeMap<u64, u64>,
}

impl ManagedResizeCorrelations {
    fn has_active(&self, pane: u64) -> bool {
        self.records
            .get(&pane)
            .is_some_and(|records| !records.is_empty())
    }

    fn has_epoch_after(&self, pane: u64, epoch: u64) -> bool {
        self.records
            .get(&pane)
            .is_some_and(|records| records.iter().any(|record| record.epoch > epoch))
    }

    fn decide_ordinary_resize(
        &mut self,
        pane: u64,
        recover_through_epoch: Option<u64>,
    ) -> OrdinaryManagedResizeDecision {
        let Some(recover_through_epoch) = recover_through_epoch else {
            return if self.has_active(pane) {
                OrdinaryManagedResizeDecision::IgnoreUnscoped
            } else {
                OrdinaryManagedResizeDecision::Configure
            };
        };
        if self.has_epoch_after(pane, recover_through_epoch) {
            return OrdinaryManagedResizeDecision::IgnoreStaleRecovery;
        }
        OrdinaryManagedResizeDecision::RecoverThrough(
            self.retire_pending_through_epoch(pane, recover_through_epoch),
        )
    }

    fn has_active_serial(&self, pane: u64, serial: Serial) -> bool {
        self.records.get(&pane).is_some_and(|records| {
            records.iter().any(|record| {
                record.serial == serial
                    && matches!(
                        record.state,
                        ManagedResizeCorrelationState::ConfigureSent
                            | ManagedResizeCorrelationState::CurrentUnsuitable
                    )
            })
        })
    }

    pub fn record_commit_observation(
        &mut self,
        pane: u64,
        serial: Serial,
        sample_size: (i32, i32),
    ) -> bool {
        let active_serial = self.has_active_serial(pane, serial);
        let observation_count = self
            .commit_observations
            .values()
            .map(VecDeque::len)
            .sum::<usize>();
        if !active_serial
            || sample_size.0 <= 0
            || sample_size.1 <= 0
            || observation_count >= MAX_MANAGED_RESIZE_SNAPSHOTS
        {
            return false;
        }
        self.commit_observations
            .entry(pane)
            .or_default()
            .push_back(ManagedCommitObservation {
                serial,
                sample_size,
            });
        true
    }

    fn take_commit_observation(&mut self, pane: u64, serial: Serial) -> Option<(i32, i32)> {
        let observations = self.commit_observations.get_mut(&pane)?;
        let position = observations
            .iter()
            .position(|observation| observation.serial == serial)?;
        let observation = observations.remove(position)?;
        if observations.is_empty() {
            self.commit_observations.remove(&pane);
        }
        Some(observation.sample_size)
    }

    fn reservation_bytes(pixel_size: (i32, i32)) -> Option<u64> {
        let width = u64::try_from(pixel_size.0).ok()?;
        let height = u64::try_from(pixel_size.1).ok()?;
        width.checked_mul(height)?.checked_mul(4)
    }

    pub fn reserve(
        &mut self,
        pane: u64,
        epoch: u64,
        pixel_size: (i32, i32),
    ) -> std::result::Result<u64, brood_protocol::ManagedResizeRejectionReason> {
        if !managed_resize_epoch_is_valid(epoch) {
            return Err(brood_protocol::ManagedResizeRejectionReason::SizeUnsatisfied);
        }
        let Some(bytes) = Self::reservation_bytes(pixel_size) else {
            return Err(brood_protocol::ManagedResizeRejectionReason::SizeUnsatisfied);
        };
        if bytes > MAX_MANAGED_RESIZE_SNAPSHOT_BYTES_PER_PANE {
            return Err(brood_protocol::ManagedResizeRejectionReason::CorrelationCapacity);
        }
        if self
            .records
            .get(&pane)
            .is_some_and(|records| records.iter().any(|record| record.epoch == epoch))
        {
            return Err(brood_protocol::ManagedResizeRejectionReason::CorrelationCapacity);
        }
        let (removed_count, removed_bytes) =
            self.records.get(&pane).map_or((0_usize, 0_u64), |records| {
                records
                    .iter()
                    .fold((0_usize, 0_u64), |(count, bytes), record| {
                        if matches!(
                            record.state,
                            ManagedResizeCorrelationState::SceneInFlight { .. }
                        ) {
                            (count, bytes)
                        } else {
                            (
                                count.saturating_add(1),
                                bytes.saturating_add(record.reserved_bytes),
                            )
                        }
                    })
            });
        let count = self
            .records
            .values()
            .map(VecDeque::len)
            .sum::<usize>()
            .saturating_sub(removed_count);
        let retained_bytes = self.reserved_bytes.saturating_sub(removed_bytes);
        if count >= MAX_MANAGED_RESIZE_SNAPSHOTS
            || retained_bytes.saturating_add(bytes) > MAX_MANAGED_RESIZE_SNAPSHOT_BYTES
        {
            return Err(brood_protocol::ManagedResizeRejectionReason::CorrelationCapacity);
        }
        if let Some(records) = self.records.get_mut(&pane) {
            records.retain(|record| {
                matches!(
                    record.state,
                    ManagedResizeCorrelationState::SceneInFlight { .. }
                )
            });
        }
        self.commit_observations.remove(&pane);
        self.records.retain(|_pane, records| !records.is_empty());
        self.reserved_bytes = retained_bytes.saturating_add(bytes);
        // Only admitted tagged authority supersedes an untagged recovery
        // wait. Its exact commit snapshot has its own correlation record.
        self.ordinary_recovery_commit_floors.remove(&pane);
        Ok(bytes)
    }

    pub fn next_surface_commit(&mut self, pane: u64) -> u64 {
        let commit = self.surface_commits.entry(pane).or_default();
        *commit = commit.saturating_add(1);
        let recovered_floor = self
            .ordinary_recovery_commit_floors
            .get(&pane)
            .copied()
            .filter(|floor| *commit > *floor);
        if let Some(recovery_floor) = recovered_floor {
            self.ordinary_recovery_commit_floors.remove(&pane);
            info!(
                pane,
                recovery_floor,
                surface_commit = *commit,
                "ordinary managed recovery commit observed"
            );
        }
        *commit
    }

    pub fn wait_for_ordinary_recovery_commit(&mut self, pane: u64) -> u64 {
        let floor = self.current_surface_commit(pane);
        self.ordinary_recovery_commit_floors.insert(pane, floor);
        floor
    }

    pub fn ordinary_recovery_commit_pending(&self) -> bool {
        !self.ordinary_recovery_commit_floors.is_empty()
    }

    /// Tagged resize requests admitted in one input drain form an epoch
    /// cohort. Exporting while only a prefix has reached `SnapshotQueued`
    /// would occupy the single in-flight scene with configure-only or mixed
    /// content and prevent the complete cohort from reaching the client.
    pub fn tagged_snapshot_cohort_incomplete(&self) -> bool {
        let mut cohorts = BTreeMap::<u64, (usize, usize)>::new();
        for record in self.records.values().flatten() {
            let queued = match record.state {
                ManagedResizeCorrelationState::ConfigureSent
                | ManagedResizeCorrelationState::CurrentUnsuitable => false,
                ManagedResizeCorrelationState::SnapshotQueued { .. } => true,
                ManagedResizeCorrelationState::SceneInFlight { .. } => continue,
            };
            let cohort = cohorts.entry(record.epoch).or_default();
            cohort.0 = cohort.0.saturating_add(1);
            cohort.1 = cohort.1.saturating_add(usize::from(queued));
        }
        cohorts.values().any(|(members, queued)| members != queued)
    }

    pub fn current_surface_commit(&self, pane: u64) -> u64 {
        self.surface_commits.get(&pane).copied().unwrap_or_default()
    }

    pub fn remove_pane(&mut self, pane: u64) -> Vec<ManagedResizeCorrelation> {
        self.surface_commits.remove(&pane);
        self.commit_observations.remove(&pane);
        self.ordinary_recovery_commit_floors.remove(&pane);
        let records = self.records.remove(&pane).unwrap_or_default();
        let records = records.into_iter().collect::<Vec<_>>();
        let bytes = records.iter().fold(0_u64, |total, record| {
            total.saturating_add(record.reserved_bytes)
        });
        self.reserved_bytes = self.reserved_bytes.saturating_sub(bytes);
        records
    }

    pub fn retire_pending_through_epoch(&mut self, pane: u64, epoch: u64) -> Vec<u64> {
        let mut released_bytes = 0_u64;
        let mut retired_epochs = Vec::new();
        if let Some(records) = self.records.get_mut(&pane) {
            records.retain(|record| {
                if matches!(
                    record.state,
                    ManagedResizeCorrelationState::SceneInFlight { .. }
                ) || record.epoch > epoch
                {
                    return true;
                }
                released_bytes = released_bytes.saturating_add(record.reserved_bytes);
                retired_epochs.push(record.epoch);
                false
            });
        }
        self.records.retain(|_pane, records| !records.is_empty());
        let active_serials = self.records.get(&pane).map_or_else(Vec::new, |records| {
            records
                .iter()
                .filter(|record| {
                    matches!(
                        record.state,
                        ManagedResizeCorrelationState::ConfigureSent
                            | ManagedResizeCorrelationState::CurrentUnsuitable
                    )
                })
                .map(|record| record.serial)
                .collect()
        });
        if let Some(observations) = self.commit_observations.get_mut(&pane) {
            observations.retain(|observation| active_serials.contains(&observation.serial));
            let empty = observations.is_empty();
            if empty {
                self.commit_observations.remove(&pane);
            }
        }
        self.reserved_bytes = self.reserved_bytes.saturating_sub(released_bytes);
        retired_epochs
    }

    pub fn complete_scene(&mut self, sequence: u64) -> Vec<(u64, u64, u64)> {
        let mut released = 0_u64;
        let mut completed_snapshots = Vec::new();
        self.records.retain(|pane, records| {
            records.retain(|record| {
                let complete = if let ManagedResizeCorrelationState::SceneInFlight {
                    surface_commit,
                    scene_sequence,
                    transport_resource,
                } = record.state
                    && scene_sequence == sequence
                {
                    info!(
                        pane = *pane,
                        epoch = record.epoch,
                        surface_commit,
                        scene_sequence,
                        transport_resource,
                        "managed resize scene presented"
                    );
                    true
                } else {
                    false
                };
                if complete {
                    released = released.saturating_add(record.reserved_bytes);
                    if let ManagedResizeCorrelationState::SceneInFlight { surface_commit, .. } =
                        record.state
                    {
                        completed_snapshots.push((*pane, record.epoch, surface_commit));
                    }
                }
                !complete
            });
            !records.is_empty()
        });
        self.reserved_bytes = self.reserved_bytes.saturating_sub(released);
        self.commit_observations.retain(|pane, _observations| {
            self.records
                .get(pane)
                .is_some_and(|records| !records.is_empty())
        });
        completed_snapshots
    }

    pub fn contains_snapshot(&self, pane: u64, epoch: u64, surface_commit: u64) -> bool {
        self.records.get(&pane).is_some_and(|records| {
            records.iter().any(|record| {
                record.epoch == epoch
                    && match record.state {
                        ManagedResizeCorrelationState::SnapshotQueued {
                            surface_commit: current,
                            ..
                        }
                        | ManagedResizeCorrelationState::SceneInFlight {
                            surface_commit: current,
                            ..
                        } => current == surface_commit,
                        ManagedResizeCorrelationState::ConfigureSent
                        | ManagedResizeCorrelationState::CurrentUnsuitable => false,
                    }
            })
        })
    }

    pub fn queued_snapshot(&self, pane: u64) -> Option<QueuedManagedSnapshot> {
        self.records.get(&pane)?.iter().rev().find_map(|record| {
            let ManagedResizeCorrelationState::SnapshotQueued {
                surface_commit,
                backing,
                ..
            } = &record.state
            else {
                return None;
            };
            Some(QueuedManagedSnapshot {
                epoch: record.epoch,
                pixel_size: record.pixel_size,
                surface_commit: *surface_commit,
                backing: backing.clone(),
            })
        })
    }

    pub fn mark_scene_in_flight(
        &mut self,
        pane: u64,
        epoch: u64,
        surface_commit: u64,
        scene_sequence: u64,
        transport_resource: u64,
    ) {
        let Some(records) = self.records.get_mut(&pane) else {
            return;
        };
        let Some(record) = records.iter_mut().find(|record| record.epoch == epoch) else {
            return;
        };
        if let ManagedResizeCorrelationState::SnapshotQueued {
            surface_commit: commit,
            committed_at,
            ..
        } = &record.state
            && *commit == surface_commit
        {
            info!(
                pane,
                epoch,
                surface_commit,
                scene_sequence,
                transport_resource,
                snapshot_to_publish_micros = committed_at.elapsed().as_micros(),
                "managed resize scene published"
            );
            record.state = ManagedResizeCorrelationState::SceneInFlight {
                surface_commit,
                scene_sequence,
                transport_resource,
            };
        }
    }
}

pub fn emit_managed_resize_status(
    fast_path: &mut PresentationFastPath,
    status: brood_protocol::ManagedResizeStatus,
) {
    let pane = status.pane;
    let epoch = status.resize_epoch;
    let result = status.result;
    let record =
        match brood_ipc::encode_managed_resize_status_record(status, fast_path.generation, epoch) {
            Ok(record) => record,
            Err(error) => {
                warn!(
                    pane,
                    epoch,
                    ?error,
                    "failed to encode managed resize status"
                );
                return;
            }
        };
    flush_managed_resize_statuses(fast_path);
    if !fast_path.deferred_managed_resize_statuses.is_empty() {
        // Input draining stops while this queue is non-empty, and one admitted
        // input can retire at most the globally bounded correlation set.
        debug_assert!(
            fast_path.deferred_managed_resize_statuses.len() < MAX_MANAGED_RESIZE_SNAPSHOTS,
            "managed resize input admission must pause at the correlation bound"
        );
        fast_path.deferred_managed_resize_statuses.push_back(record);
        return;
    }
    if let Err(record) = fast_path.frames.try_send(record) {
        warn!(
            pane,
            epoch,
            opcode = record.opcode,
            "managed resize status lane is full"
        );
        // See the admission proof above: no second input is decoded until all
        // retained statuses from this bounded correlation batch are flushed.
        debug_assert!(
            fast_path.deferred_managed_resize_statuses.len() < MAX_MANAGED_RESIZE_SNAPSHOTS,
            "managed resize input admission must pause at the correlation bound"
        );
        fast_path.deferred_managed_resize_statuses.push_back(record);
        return;
    }
    if let Err(error) = fast_path.frame_notifications.notify() {
        warn!(
            pane,
            epoch,
            ?error,
            "failed to notify managed resize status"
        );
        return;
    }
    info!(pane, epoch, ?result, "managed resize status emitted");
}

pub fn flush_managed_resize_statuses(fast_path: &mut PresentationFastPath) {
    let published = flush_retained_critical_records(
        &mut fast_path.frames,
        &fast_path.frame_notifications,
        &mut fast_path.deferred_managed_resize_statuses,
    );
    if let Err(error) = published {
        warn!(?error, "failed to notify retained managed resize statuses");
    }
}

fn flush_retained_critical_records(
    producer: &mut brood_ipc::CriticalProducer,
    notifications: &brood_ipc::EventNotifier,
    retained: &mut VecDeque<brood_ipc::CriticalRecord>,
) -> std::result::Result<usize, brood_ipc::CriticalLaneError> {
    let mut published = false;
    let mut count = 0;
    while let Some(record) = retained.pop_front() {
        if let Err(record) = producer.try_send(record) {
            retained.push_front(record);
            break;
        }
        published = true;
        count += 1;
    }
    if published {
        notifications.notify()?;
    }
    Ok(count)
}

#[allow(dead_code)]
pub struct DmabufRuntime {
    pub state: DmabufState,
    pub global: Option<DmabufGlobal>,
    pub latest_client_buffer: Option<Dmabuf>,
    pub latest_client_buffer_sequence: u64,
    pub fast_path: Option<PresentationFastPath>,
    pub fast_path_input_draining: bool,
    pub default_feedback: Option<DmabufFeedback>,
    pub exporter: Option<crate::frame_export::SharedFrameExporter>,
    pub stats: DmabufStats,
    pub shm_formats: usize,
    pub advertised_formats: usize,
    pub protocol_version: Option<u32>,
    pub render_node: Option<String>,
    pub egl_wl_bind_status: String,
    pub disabled_reason: Option<&'static str>,
    pub explicit_sync_state: Option<DrmSyncobjState>,
    pub explicit_sync_protocol_version: Option<u32>,
    pub explicit_sync_disabled_reason: &'static str,
}

impl Default for DmabufRuntime {
    fn default() -> Self {
        Self {
            state: DmabufState::new(),
            global: None,
            latest_client_buffer: None,
            latest_client_buffer_sequence: 0,
            fast_path: None,
            fast_path_input_draining: false,
            default_feedback: None,
            exporter: None,
            stats: DmabufStats::default(),
            shm_formats: 0,
            advertised_formats: 0,
            protocol_version: None,
            render_node: None,
            egl_wl_bind_status: "uninitialized".to_string(),
            disabled_reason: None,
            explicit_sync_state: None,
            explicit_sync_protocol_version: None,
            explicit_sync_disabled_reason: "headless_exporter_no_drm_import_device",
        }
    }
}

#[allow(dead_code)]
#[derive(Default)]
pub struct DmabufStats {
    pub attempts: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub last_import: Option<String>,
    pub last_error: Option<String>,
    pub explicit_sync_release_points: u64,
    pub explicit_sync_blockers: u64,
    pub implicit_dmabuf_blockers: u64,
    pub blocker_install_failures: u64,
    pub last_blocker_error: Option<String>,
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        warn!(
            ?client_id,
            ?reason,
            "Wayland client disconnected from Brood"
        );
    }
}

#[cfg(test)]
mod managed_resize_tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "managed-resize fixtures construct exact pane and epoch entries before asserting their states"
    )]

    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn managed_resize_statuses_survive_a_saturated_frame_ring_in_order() {
        const GENERATION: u64 = 17;
        let file = brood_ipc::CriticalLaneFile::create(GENERATION).unwrap();
        let mut producer = brood_ipc::CriticalProducer::map(file.as_fd(), GENERATION).unwrap();
        let mut consumer = brood_ipc::CriticalConsumer::map(file.as_fd(), GENERATION).unwrap();
        let notifications = brood_ipc::EventNotifier::create().unwrap();
        for sequence in 0..brood_ipc::CRITICAL_CAPACITY as u64 {
            producer
                .try_send(brood_ipc::CriticalRecord {
                    sequence,
                    ..Default::default()
                })
                .unwrap();
        }
        let mut retained = (100..110)
            .map(|sequence| {
                brood_ipc::encode_managed_resize_status_record(
                    brood_protocol::ManagedResizeStatus {
                        pane: 7,
                        resize_epoch: sequence,
                        result: brood_protocol::ManagedResizeStatusResult::Rejected {
                            reason: brood_protocol::ManagedResizeRejectionReason::PaneUnavailable,
                        },
                    },
                    GENERATION,
                    sequence,
                )
                .unwrap()
            })
            .collect::<VecDeque<_>>();

        assert_eq!(
            flush_retained_critical_records(&mut producer, &notifications, &mut retained).unwrap(),
            0
        );
        assert_eq!(retained.len(), 10);
        for _ in 0..brood_ipc::CRITICAL_CAPACITY {
            drop(consumer.try_recv().unwrap());
        }
        assert_eq!(
            flush_retained_critical_records(&mut producer, &notifications, &mut retained).unwrap(),
            10
        );
        assert!(retained.is_empty());
        for sequence in 100..110 {
            let record = consumer.try_recv().unwrap();
            let status = brood_ipc::decode_managed_resize_status_record(&record).unwrap();
            assert_eq!(status.pane, 7);
            assert_eq!(status.resize_epoch, sequence);
            assert_eq!(
                status.result,
                brood_protocol::ManagedResizeStatusResult::Rejected {
                    reason: brood_protocol::ManagedResizeRejectionReason::PaneUnavailable,
                }
            );
        }
    }

    fn correlation(
        epoch: u64,
        reserved_bytes: u64,
        state: ManagedResizeCorrelationState,
    ) -> ManagedResizeCorrelation {
        ManagedResizeCorrelation {
            epoch,
            serial: Serial::from(u32::try_from(epoch).unwrap_or(u32::MAX)),
            logical_size: (200, 100),
            pixel_size: (300, 150),
            sent_after_commit: 4,
            reserved_bytes,
            configured_at: Instant::now(),
            state,
        }
    }

    fn reserve_and_insert(correlations: &mut ManagedResizeCorrelations, pane: u64, epoch: u64) {
        let bytes = correlations.reserve(pane, epoch, (1, 1)).unwrap();
        correlations
            .records
            .entry(pane)
            .or_default()
            .push_back(correlation(
                epoch,
                bytes,
                ManagedResizeCorrelationState::ConfigureSent,
            ));
    }

    fn queue_snapshot(
        correlations: &mut ManagedResizeCorrelations,
        pane: u64,
        epoch: u64,
        surface_commit: u64,
    ) {
        let record = correlations
            .records
            .get_mut(&pane)
            .and_then(|records| records.iter_mut().find(|record| record.epoch == epoch))
            .unwrap();
        record.state = ManagedResizeCorrelationState::SnapshotQueued {
            surface_commit,
            committed_at: Instant::now(),
            backing: crate::frame_export::ManagedSnapshotBacking::SharedMemory(Arc::<[u8]>::from(
                [u8::try_from(pane).unwrap_or_default()],
            )),
        };
    }

    #[test]
    fn tagged_snapshot_cohort_exports_only_after_every_member_is_queued() {
        let mut correlations = ManagedResizeCorrelations::default();
        reserve_and_insert(&mut correlations, 1, 7);
        reserve_and_insert(&mut correlations, 2, 7);
        let reserved = correlations.reserved_bytes;
        assert!(correlations.tagged_snapshot_cohort_incomplete());

        queue_snapshot(&mut correlations, 2, 7, 12);
        assert!(correlations.tagged_snapshot_cohort_incomplete());
        assert!(correlations.queued_snapshot(1).is_none());
        assert_eq!(correlations.queued_snapshot(2).unwrap().epoch, 7);

        queue_snapshot(&mut correlations, 1, 7, 11);
        assert!(!correlations.tagged_snapshot_cohort_incomplete());
        assert_eq!(correlations.reserved_bytes, reserved);
        assert_eq!(correlations.queued_snapshot(1).unwrap().surface_commit, 11);
        assert_eq!(correlations.queued_snapshot(2).unwrap().surface_commit, 12);

        correlations.mark_scene_in_flight(1, 7, 11, 41, 101);
        correlations.mark_scene_in_flight(2, 7, 12, 41, 102);
        assert!(!correlations.tagged_snapshot_cohort_incomplete());
        for (pane, resource) in [(1, 101), (2, 102)] {
            assert!(matches!(
                correlations.records[&pane][0].state,
                ManagedResizeCorrelationState::SceneInFlight {
                    scene_sequence: 41,
                    transport_resource,
                    ..
                } if transport_resource == resource
            ));
        }
    }

    #[test]
    fn tagged_snapshot_cohort_handles_one_pane_mixed_epochs_and_retirement() {
        let mut correlations = ManagedResizeCorrelations::default();
        reserve_and_insert(&mut correlations, 1, 6);
        assert!(correlations.tagged_snapshot_cohort_incomplete());
        queue_snapshot(&mut correlations, 1, 6, 9);
        assert!(!correlations.tagged_snapshot_cohort_incomplete());

        reserve_and_insert(&mut correlations, 2, 7);
        assert!(correlations.tagged_snapshot_cohort_incomplete());
        assert_eq!(correlations.retire_pending_through_epoch(2, 7), vec![7]);
        assert!(!correlations.tagged_snapshot_cohort_incomplete());
        assert_eq!(correlations.queued_snapshot(1).unwrap().epoch, 6);

        correlations.mark_scene_in_flight(1, 6, 9, 40, 100);
        reserve_and_insert(&mut correlations, 1, 8);
        assert!(correlations.tagged_snapshot_cohort_incomplete());
        queue_snapshot(&mut correlations, 1, 8, 10);
        assert!(!correlations.tagged_snapshot_cohort_incomplete());
        assert!(matches!(
            correlations.records[&1][0].state,
            ManagedResizeCorrelationState::SceneInFlight {
                scene_sequence: 40,
                transport_resource: 100,
                ..
            }
        ));
        assert_eq!(correlations.queued_snapshot(1).unwrap().epoch, 8);
    }

    #[test]
    fn managed_resize_reservation_has_absolute_count_and_per_pane_byte_bounds() {
        let mut correlations = ManagedResizeCorrelations::default();
        assert_eq!(
            correlations.reserve(1, brood_protocol::MAX_MANAGED_RESIZE_EPOCH + 1, (1, 1)),
            Err(brood_protocol::ManagedResizeRejectionReason::SizeUnsatisfied)
        );
        for pane in 1..=MAX_MANAGED_RESIZE_SNAPSHOTS as u64 {
            reserve_and_insert(&mut correlations, pane, pane);
        }
        assert_eq!(
            correlations.reserve(1000, 1000, (1, 1)),
            Err(brood_protocol::ManagedResizeRejectionReason::CorrelationCapacity)
        );
        let mut correlations = ManagedResizeCorrelations::default();
        assert_eq!(
            correlations.reserve(1, 1, (4097, 4096)),
            Err(brood_protocol::ManagedResizeRejectionReason::CorrelationCapacity)
        );
    }

    #[test]
    fn failed_reservation_does_not_discard_an_older_queued_snapshot() {
        let mut correlations = ManagedResizeCorrelations::default();
        reserve_and_insert(&mut correlations, 1, 1);
        for (pane, bytes) in [
            (2, 64 * 1024 * 1024),
            (3, 64 * 1024 * 1024),
            (4, 64 * 1024 * 1024),
            (5, 64 * 1024 * 1024 - 4),
        ] {
            correlations
                .records
                .entry(pane)
                .or_default()
                .push_back(correlation(
                    pane,
                    bytes,
                    ManagedResizeCorrelationState::SceneInFlight {
                        surface_commit: pane,
                        scene_sequence: 8,
                        transport_resource: pane,
                    },
                ));
            correlations.reserved_bytes = correlations.reserved_bytes.saturating_add(bytes);
        }
        assert_eq!(
            correlations.reserve(1, 3, (4096, 4096)),
            Err(brood_protocol::ManagedResizeRejectionReason::CorrelationCapacity)
        );
        assert_eq!(
            correlations.records[&1].front().map(|record| record.epoch),
            Some(1)
        );
        assert_eq!(
            correlations.reserve(1, 1, (1, 1)),
            Err(brood_protocol::ManagedResizeRejectionReason::CorrelationCapacity)
        );
    }

    #[test]
    fn newer_request_replaces_only_not_published_snapshot() {
        let mut correlations = ManagedResizeCorrelations::default();
        reserve_and_insert(&mut correlations, 1, 1);
        correlations.records.get_mut(&1).unwrap()[0].state =
            ManagedResizeCorrelationState::SceneInFlight {
                surface_commit: 5,
                scene_sequence: 11,
                transport_resource: 17,
            };
        reserve_and_insert(&mut correlations, 1, 2);
        reserve_and_insert(&mut correlations, 1, 3);
        let epochs = correlations.records[&1]
            .iter()
            .map(|record| record.epoch)
            .collect::<Vec<_>>();
        assert_eq!(epochs, vec![1, 3]);
    }

    #[test]
    fn scoped_ordinary_recovery_retires_through_authority_and_keeps_newer_and_in_flight() {
        let mut correlations = ManagedResizeCorrelations::default();
        let bytes = ManagedResizeCorrelations::reservation_bytes((300, 150)).unwrap();
        let records = correlations.records.entry(1).or_default();
        records.push_back(correlation(
            1,
            bytes,
            ManagedResizeCorrelationState::SceneInFlight {
                surface_commit: 5,
                scene_sequence: 11,
                transport_resource: 17,
            },
        ));
        records.push_back(correlation(
            2,
            bytes,
            ManagedResizeCorrelationState::ConfigureSent,
        ));
        records.push_back(correlation(
            3,
            bytes,
            ManagedResizeCorrelationState::CurrentUnsuitable,
        ));
        records.push_back(correlation(
            4,
            bytes,
            ManagedResizeCorrelationState::SnapshotQueued {
                surface_commit: 9,
                committed_at: Instant::now(),
                backing: crate::frame_export::ManagedSnapshotBacking::SharedMemory(
                    Arc::<[u8]>::from([]),
                ),
            },
        ));
        records.push_back(correlation(
            5,
            bytes,
            ManagedResizeCorrelationState::ConfigureSent,
        ));
        correlations.reserved_bytes = bytes * 5;
        let retired = correlations.retire_pending_through_epoch(1, 4);
        assert_eq!(retired, vec![2, 3, 4]);
        assert_eq!(correlations.records[&1].len(), 2);
        assert_eq!(correlations.records[&1][0].epoch, 1);
        assert_eq!(correlations.records[&1][1].epoch, 5);
        assert_eq!(correlations.reserved_bytes, bytes * 2);
        assert!(matches!(
            correlations.records[&1][0].state,
            ManagedResizeCorrelationState::SceneInFlight { .. }
        ));
    }

    #[test]
    fn stale_recovery_is_pane_scoped_and_cannot_retire_a_newer_exact_correlation() {
        let mut correlations = ManagedResizeCorrelations::default();
        reserve_and_insert(&mut correlations, 1, 1);
        reserve_and_insert(&mut correlations, 1, 2);
        reserve_and_insert(&mut correlations, 2, 2);

        assert!(correlations.has_epoch_after(1, 1));
        assert_eq!(
            correlations.retire_pending_through_epoch(1, 1),
            Vec::<u64>::new()
        );
        assert_eq!(
            correlations.records[&1]
                .iter()
                .map(|record| record.epoch)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(correlations.records[&2][0].epoch, 2);

        for pane in [1, 2] {
            let record = correlations
                .records
                .get_mut(&pane)
                .unwrap()
                .front_mut()
                .unwrap();
            record.state = ManagedResizeCorrelationState::SnapshotQueued {
                surface_commit: 7,
                committed_at: Instant::now(),
                backing: crate::frame_export::ManagedSnapshotBacking::SharedMemory(
                    Arc::<[u8]>::from([]),
                ),
            };
        }
        assert_eq!(
            correlations
                .queued_snapshot(1)
                .map(|snapshot| snapshot.epoch),
            Some(2)
        );
        assert_eq!(
            correlations
                .queued_snapshot(2)
                .map(|snapshot| snapshot.epoch),
            Some(2)
        );
        for pane in [1, 2] {
            correlations.mark_scene_in_flight(pane, 2, 7, 77, pane + 100);
        }
        let mut completed = correlations.complete_scene(77);
        completed.sort_unstable();
        assert_eq!(completed, vec![(1, 2, 7), (2, 2, 7)]);
        assert!(correlations.records.is_empty());
    }

    #[test]
    fn control_authority_drives_bare_suppression_stale_ignore_and_scoped_retirement() {
        assert_eq!(
            managed_resize_request_authority(None, None),
            Some(ManagedResizeRequestAuthority::Ordinary)
        );
        assert_eq!(
            managed_resize_request_authority(None, Some(1)),
            Some(ManagedResizeRequestAuthority::RecoverThrough(1))
        );
        assert_eq!(
            managed_resize_request_authority(Some(2), None),
            Some(ManagedResizeRequestAuthority::Tagged(2))
        );
        assert_eq!(managed_resize_request_authority(Some(2), Some(1)), None);

        let bytes = ManagedResizeCorrelations::reservation_bytes((300, 150)).unwrap();
        let mut correlations = ManagedResizeCorrelations::default();
        correlations.records.entry(7).or_default().extend([
            correlation(1, bytes, ManagedResizeCorrelationState::ConfigureSent),
            correlation(2, bytes, ManagedResizeCorrelationState::CurrentUnsuitable),
        ]);
        correlations.reserved_bytes = bytes * 2;

        assert_eq!(
            correlations.decide_ordinary_resize(7, None),
            OrdinaryManagedResizeDecision::IgnoreUnscoped
        );
        assert_eq!(correlations.records[&7].len(), 2);
        assert_eq!(
            correlations.decide_ordinary_resize(7, Some(1)),
            OrdinaryManagedResizeDecision::IgnoreStaleRecovery
        );
        assert_eq!(correlations.reserved_bytes, bytes * 2);
        assert_eq!(
            correlations.records[&7]
                .iter()
                .map(|record| record.epoch)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(matches!(
            correlations.records[&7][0].state,
            ManagedResizeCorrelationState::ConfigureSent
        ));
        assert!(matches!(
            correlations.records[&7][1].state,
            ManagedResizeCorrelationState::CurrentUnsuitable
        ));
        assert_eq!(
            correlations.decide_ordinary_resize(7, Some(2)),
            OrdinaryManagedResizeDecision::RecoverThrough(vec![1, 2])
        );
        assert!(!correlations.has_active(7));
        assert_eq!(correlations.reserved_bytes, 0);
        assert_eq!(
            correlations.decide_ordinary_resize(7, None),
            OrdinaryManagedResizeDecision::Configure
        );
    }

    #[test]
    fn scoped_recovery_forces_one_configure_while_ordinary_deduplicates_current_size() {
        assert!(PresentationConfigurePolicy::Force.should_emit(true));
        assert!(PresentationConfigurePolicy::Force.should_emit(false));
        assert!(!PresentationConfigurePolicy::Deduplicate.should_emit(true));
        assert!(PresentationConfigurePolicy::Deduplicate.should_emit(false));
    }

    #[test]
    fn presentation_configure_state_is_tiled_and_preserves_other_states() {
        let size: Size<i32, Logical> = (480, 686).into();
        let mut ordinary = smithay::wayland::shell::xdg::ToplevelState::default();
        ordinary.states.set(xdg_toplevel::State::Activated);
        ordinary.states.set(xdg_toplevel::State::Maximized);
        ordinary.states.set(xdg_toplevel::State::Fullscreen);
        apply_presentation_toplevel_state(&mut ordinary, size);

        let mut tagged = ordinary.clone();
        apply_presentation_toplevel_state(&mut tagged, size);

        assert_eq!(ordinary, tagged);
        assert!(presentation_toplevel_state_matches(&ordinary, size));
        assert!(ordinary.states.contains(xdg_toplevel::State::Activated));
        assert!(ordinary.states.contains(xdg_toplevel::State::Maximized));
        assert!(ordinary.states.contains(xdg_toplevel::State::Fullscreen));
        assert!(ordinary.states.contains(xdg_toplevel::State::TiledLeft));
        assert!(ordinary.states.contains(xdg_toplevel::State::TiledRight));
        assert!(ordinary.states.contains(xdg_toplevel::State::TiledTop));
        assert!(ordinary.states.contains(xdg_toplevel::State::TiledBottom));
    }

    #[test]
    fn two_pane_recovery_requires_both_later_same_pane_commits() {
        let mut correlations = ManagedResizeCorrelations::default();
        for _ in 0..4 {
            correlations.next_surface_commit(1);
        }
        for _ in 0..5 {
            correlations.next_surface_commit(2);
        }
        reserve_and_insert(&mut correlations, 1, 7);
        reserve_and_insert(&mut correlations, 2, 7);

        assert_eq!(correlations.retire_pending_through_epoch(1, 7), vec![7]);
        assert_eq!(correlations.retire_pending_through_epoch(2, 7), vec![7]);
        assert_eq!(correlations.wait_for_ordinary_recovery_commit(1), 4);
        assert_eq!(correlations.wait_for_ordinary_recovery_commit(2), 5);
        assert!(correlations.records.is_empty());
        assert!(correlations.ordinary_recovery_commit_pending());

        assert_eq!(correlations.next_surface_commit(3), 1);
        assert!(correlations.ordinary_recovery_commit_pending());
        assert_eq!(correlations.next_surface_commit(1), 5);
        assert!(correlations.ordinary_recovery_commit_pending());
        assert_eq!(correlations.next_surface_commit(2), 6);
        assert!(!correlations.ordinary_recovery_commit_pending());
    }

    #[test]
    fn retired_pending_recovery_arms_until_one_later_same_pane_commit() {
        let mut correlations = ManagedResizeCorrelations::default();
        assert_eq!(correlations.next_surface_commit(1), 1);
        reserve_and_insert(&mut correlations, 1, 7);
        assert_eq!(
            correlations.decide_ordinary_resize(1, Some(7)),
            OrdinaryManagedResizeDecision::RecoverThrough(vec![7])
        );
        assert_eq!(correlations.wait_for_ordinary_recovery_commit(1), 1);
        assert!(correlations.ordinary_recovery_commit_pending());

        // The redraw gate remains closed until this exact pane advances.
        assert_eq!(correlations.next_surface_commit(2), 1);
        assert!(correlations.ordinary_recovery_commit_pending());
        assert_eq!(correlations.next_surface_commit(1), 2);
        assert!(!correlations.ordinary_recovery_commit_pending());
    }

    #[test]
    fn scene_in_flight_recovery_preserves_scene_and_still_arms_commit_floor() {
        let mut correlations = ManagedResizeCorrelations::default();
        assert_eq!(correlations.next_surface_commit(1), 1);
        let bytes = ManagedResizeCorrelations::reservation_bytes((300, 150)).unwrap();
        correlations
            .records
            .entry(1)
            .or_default()
            .push_back(correlation(
                7,
                bytes,
                ManagedResizeCorrelationState::SceneInFlight {
                    surface_commit: 1,
                    scene_sequence: 11,
                    transport_resource: 17,
                },
            ));
        correlations.reserved_bytes = bytes;
        assert_eq!(
            correlations.decide_ordinary_resize(1, Some(7)),
            OrdinaryManagedResizeDecision::RecoverThrough(Vec::new())
        );
        assert!(matches!(
            correlations.records[&1][0].state,
            ManagedResizeCorrelationState::SceneInFlight { .. }
        ));
        assert_eq!(correlations.wait_for_ordinary_recovery_commit(1), 1);
        assert!(correlations.ordinary_recovery_commit_pending());
        assert_eq!(correlations.next_surface_commit(1), 2);
        assert!(!correlations.ordinary_recovery_commit_pending());
        assert_eq!(correlations.records[&1][0].epoch, 7);
    }

    #[test]
    fn no_record_and_idempotent_recovery_rearm_the_current_commit_floor() {
        let mut correlations = ManagedResizeCorrelations::default();
        for _ in 0..2 {
            assert_eq!(
                correlations.decide_ordinary_resize(1, Some(7)),
                OrdinaryManagedResizeDecision::RecoverThrough(Vec::new())
            );
            assert_eq!(correlations.wait_for_ordinary_recovery_commit(1), 0);
        }
        assert!(correlations.ordinary_recovery_commit_pending());
        assert_eq!(correlations.next_surface_commit(2), 1);
        assert!(correlations.ordinary_recovery_commit_pending());
        assert_eq!(correlations.next_surface_commit(1), 1);
        assert!(!correlations.ordinary_recovery_commit_pending());

        assert_eq!(
            correlations.decide_ordinary_resize(1, Some(7)),
            OrdinaryManagedResizeDecision::RecoverThrough(Vec::new())
        );
        assert_eq!(correlations.wait_for_ordinary_recovery_commit(1), 1);
        correlations.remove_pane(1);
        assert!(!correlations.ordinary_recovery_commit_pending());

        assert_eq!(correlations.next_surface_commit(1), 1);
        assert_eq!(correlations.wait_for_ordinary_recovery_commit(1), 1);
        assert_eq!(
            correlations.reserve(1, 7, (i32::MAX, i32::MAX)),
            Err(brood_protocol::ManagedResizeRejectionReason::CorrelationCapacity)
        );
        assert!(correlations.ordinary_recovery_commit_pending());
        correlations.reserve(1, 7, (300, 150)).unwrap();
        assert!(!correlations.ordinary_recovery_commit_pending());
    }

    #[test]
    fn active_managed_correlation_suppresses_ordinary_renegotiation() {
        let states = [
            ManagedResizeCorrelationState::ConfigureSent,
            ManagedResizeCorrelationState::CurrentUnsuitable,
            ManagedResizeCorrelationState::SnapshotQueued {
                surface_commit: 9,
                committed_at: Instant::now(),
                backing: crate::frame_export::ManagedSnapshotBacking::SharedMemory(
                    Arc::<[u8]>::from([]),
                ),
            },
            ManagedResizeCorrelationState::SceneInFlight {
                surface_commit: 9,
                scene_sequence: 20,
                transport_resource: 21,
            },
        ];
        for state in states {
            let mut correlations = ManagedResizeCorrelations::default();
            correlations
                .records
                .entry(1)
                .or_default()
                .push_back(correlation(1, 4, state));
            assert!(correlations.has_active(1));
            correlations.remove_pane(1);
            assert!(!correlations.has_active(1));
        }

        let mut correlations = ManagedResizeCorrelations::default();
        correlations
            .records
            .entry(1)
            .or_default()
            .push_back(correlation(
                1,
                4,
                ManagedResizeCorrelationState::SceneInFlight {
                    surface_commit: 9,
                    scene_sequence: 20,
                    transport_resource: 21,
                },
            ));
        correlations.reserved_bytes = 4;
        correlations.complete_scene(20);
        assert!(!correlations.has_active(1));

        assert!(managed_resize_suppresses_ordinary_renegotiation(
            true, false
        ));
        assert!(managed_resize_suppresses_ordinary_renegotiation(
            false, true
        ));
        assert!(!managed_resize_suppresses_ordinary_renegotiation(
            false, false
        ));
    }

    #[test]
    fn pre_commit_observations_are_serial_bound_bounded_and_consumed_once() {
        let mut correlations = ManagedResizeCorrelations::default();
        reserve_and_insert(&mut correlations, 1, 1);
        let serial = Serial::from(1);
        assert!(!correlations.record_commit_observation(2, serial, (300, 150)));
        assert!(!correlations.record_commit_observation(1, Serial::from(2), (300, 150)));
        assert!(correlations.record_commit_observation(1, serial, (300, 150)));
        assert!(correlations.record_commit_observation(1, serial, (301, 151)));
        assert_eq!(
            correlations.take_commit_observation(1, serial),
            Some((300, 150))
        );
        assert_eq!(
            correlations.take_commit_observation(1, serial),
            Some((301, 151))
        );
        assert_eq!(correlations.take_commit_observation(1, serial), None);

        for pane in 1..=MAX_MANAGED_RESIZE_SNAPSHOTS as u64 {
            if pane != 1 {
                reserve_and_insert(&mut correlations, pane, pane);
            }
            assert!(correlations.record_commit_observation(
                pane,
                Serial::from(u32::try_from(pane).unwrap()),
                (1, 1),
            ));
        }
        assert!(!correlations.record_commit_observation(1, serial, (2, 2)));
    }

    #[test]
    fn pre_commit_observation_uses_ack_not_prior_committed_serial() {
        let mut correlations = ManagedResizeCorrelations::default();
        reserve_and_insert(&mut correlations, 1, 23);
        assert!(!correlations.record_commit_observation(1, Serial::from(21), (311, 344),));
        assert!(correlations.record_commit_observation(1, Serial::from(23), (311, 344),));
        assert_eq!(
            correlations.take_commit_observation(1, Serial::from(23)),
            Some((311, 344))
        );
    }

    #[test]
    fn correlation_cleanup_removes_pre_commit_observations() {
        let mut correlations = ManagedResizeCorrelations::default();
        reserve_and_insert(&mut correlations, 1, 1);
        assert!(correlations.record_commit_observation(1, Serial::from(1), (300, 150)));
        reserve_and_insert(&mut correlations, 1, 2);
        assert_eq!(
            correlations.take_commit_observation(1, Serial::from(1)),
            None
        );
        assert!(correlations.record_commit_observation(1, Serial::from(2), (300, 150)));
        correlations.retire_pending_through_epoch(1, 2);
        assert_eq!(
            correlations.take_commit_observation(1, Serial::from(2)),
            None
        );

        reserve_and_insert(&mut correlations, 1, 3);
        assert!(correlations.record_commit_observation(1, Serial::from(3), (300, 150)));
        correlations.remove_pane(1);
        assert_eq!(
            correlations.take_commit_observation(1, Serial::from(3)),
            None
        );
    }

    #[test]
    fn exact_commit_requires_serial_new_buffer_and_both_sizes() {
        let record = correlation(7, 180_000, ManagedResizeCorrelationState::ConfigureSent);
        assert!(managed_commit_is_suitable(
            &record,
            Some(Serial::from(7)),
            5,
            Some((200, 100)),
            Some((300, 150)),
        ));
        assert!(!managed_commit_is_suitable(
            &record,
            Some(Serial::from(6)),
            5,
            Some((200, 100)),
            Some((300, 150)),
        ));
        assert!(!managed_commit_is_suitable(
            &record,
            Some(Serial::from(7)),
            5,
            Some((199, 100)),
            Some((300, 150)),
        ));
        assert!(!managed_commit_is_suitable(
            &record,
            Some(Serial::from(7)),
            5,
            Some((200, 100)),
            None,
        ));
        assert!(!managed_commit_is_suitable(
            &record,
            Some(Serial::from(7)),
            5,
            Some((200, 100)),
            Some((299, 150)),
        ));
    }

    #[test]
    fn snapshot_is_owned_until_the_exact_scene_is_presented() {
        let mut correlations = ManagedResizeCorrelations::default();
        let bytes = correlations.reserve(1, 1, (300, 150)).unwrap();
        correlations
            .records
            .entry(1)
            .or_default()
            .push_back(correlation(
                1,
                bytes,
                ManagedResizeCorrelationState::SnapshotQueued {
                    surface_commit: 9,
                    committed_at: Instant::now(),
                    backing: crate::frame_export::ManagedSnapshotBacking::SharedMemory(
                        Arc::<[u8]>::from([]),
                    ),
                },
            ));
        correlations.mark_scene_in_flight(1, 1, 9, 20, 31);
        assert!(matches!(
            correlations.records[&1][0].state,
            ManagedResizeCorrelationState::SceneInFlight {
                surface_commit: 9,
                scene_sequence: 20,
                transport_resource: 31,
            }
        ));
        correlations.complete_scene(19);
        assert_eq!(correlations.records[&1].len(), 1);
        correlations.complete_scene(20);
        assert!(correlations.records.is_empty());
        assert_eq!(correlations.reserved_bytes, 0);
    }

    #[test]
    fn fractional_scale_uses_protocol_denominator() {
        assert_eq!(scale_120_for_fractional(1.0), 120);
        assert_eq!(scale_120_for_fractional(1.5), 180);
        assert_eq!(scale_120_for_fractional(f64::NAN), 120);
        assert!(pixel_size_matches_scale((200, 100), (300, 150), 180));
        assert!(pixel_size_matches_scale((207, 229), (310, 343), 180));
        assert!(!pixel_size_matches_scale((207, 229), (312, 343), 180));
        assert!(!pixel_size_matches_scale((207, 229), (310, 343), 0));
        assert!(!pixel_size_matches_scale((0, 229), (310, 343), 180));
        assert!(!pixel_size_matches_scale((207, 229), (0, 343), 180));
        assert!(!pixel_size_matches_scale(
            (i32::MAX, i32::MAX),
            (i32::MAX, i32::MAX),
            1,
        ));
    }

    #[test]
    fn removal_rejects_only_epochs_without_a_committed_snapshot() {
        assert!(managed_resize_needs_cleanup_rejection(
            &ManagedResizeCorrelationState::ConfigureSent
        ));
        assert!(managed_resize_needs_cleanup_rejection(
            &ManagedResizeCorrelationState::CurrentUnsuitable
        ));
        assert!(!managed_resize_needs_cleanup_rejection(
            &ManagedResizeCorrelationState::SnapshotQueued {
                surface_commit: 9,
                committed_at: Instant::now(),
                backing: crate::frame_export::ManagedSnapshotBacking::SharedMemory(
                    Arc::<[u8]>::from([]),
                ),
            }
        ));
        assert!(!managed_resize_needs_cleanup_rejection(
            &ManagedResizeCorrelationState::SceneInFlight {
                surface_commit: 9,
                scene_sequence: 10,
                transport_resource: 21,
            }
        ));
    }
}
