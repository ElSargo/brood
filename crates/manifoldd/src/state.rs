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
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
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
            timer::{TimeoutAction, Timer},
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_output, wl_surface::WlSurface},
        },
    },
    utils::{IsAlive, Logical, Point, SERIAL_COUNTER, Size, Transform},
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
use tracing::{info, warn};

use crate::{
    MANIFOLD_HOST_DBUS_ADDRESS_ENV, MANIFOLD_PANE_ID_ENV, MANIFOLD_SESSION_ENV,
    MANIFOLDD_DBUS_ADDRESS_ENV, MANIFOLDD_DBUS_DESTINATION_ENV,
    browser::{BrowserInstanceSummary, BrowserLaunchOptions, BrowserManager},
    clipboard::HostClipboardBridge,
    command::{SurfaceCommand, WorkspaceCommand},
    geometry::{WindowSizeNegotiation, negotiate_window_size},
    plugin_runtime::{
        PluginActorSummary, PluginHostEvent, PluginHostMessage, PluginMailboxSummary, PluginRuntime,
    },
    session_bus::{SessionBusHandle, SessionBusRealm},
    state_services::{FocusService, SurfaceSessionService, TestHarnessService},
    wasm_test::service_request,
};
use manifold_wasm::{TestHarnessCompletion, TestHarnessOutcomeKind, TestHarnessRuntime};

#[cfg(feature = "shm-fallback")]
const MANDATORY_SHM_FORMATS: usize = 2;
const WINDOW_LIFECYCLE_REFRESH_INTERVAL: Duration = Duration::from_millis(50);
const SESSION_INIT_ACTOR_PREFIX: &str = "manifold.session-init";
pub const SESSION_OUTPUT_REFRESH_MILLIHZ: i32 = 60_000;
const MANIFOLD_SURFACE_SESSION_ID_ENV: &str = "MANIFOLD_SURFACE_SESSION_ID";
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationWindowSummary {
    pub pane: u64,
    pub session: u64,
    pub app_id: String,
}

pub type ExtensionMetadata = BTreeMap<String, String>;
pub const MANIFOLD_STATUS_PUBLISHER: &str = "manifold";

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

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConfiguredWorkspacePlugin {
    path: PathBuf,
    writable: bool,
    startup_message: Option<String>,
    lifecycle: Option<ConfiguredPluginLifecycle>,
    resources: ConfiguredPluginResources,
    runtime_io: manifold_wasm::RuntimeIoCapabilities,
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

pub struct Manifold {
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
    pub stats: RuntimeStats,
    next_window_slot: usize,
    last_window_count: usize,
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
    wasm_registry: Arc<tokio::sync::Mutex<manifold_wasm::Registry>>,
    wasm_host_mailbox: Option<manifold_wasm::Mailbox>,
    legacy_wasm_actor_generation: Option<manifold_wasm::LegacyWasmActorGeneration>,
    wasm_io_actors: BTreeMap<String, manifold_wasm::HostedWasmActor>,
    session_init_actor: Option<PathBuf>,
    realm_inits: BTreeMap<SurfaceSessionId, RealmInitState>,
    configured_workspace_plugins: BTreeMap<String, ConfiguredWorkspacePlugin>,
    configured_plugin_lifecycles: BTreeMap<String, ConfiguredPluginLifecycle>,
    configured_plugin_resources: BTreeMap<String, ConfiguredPluginResources>,
    configured_plugin_runtime_io: BTreeMap<String, manifold_wasm::RuntimeIoCapabilities>,
    filesystem_watcher_service: manifold_wasm::NativeFilesystemWatcherService,
    pub(crate) test_surface_markers: bool,
    pub(crate) test_harness: TestHarnessService,
    pub(crate) manifold_session: Option<String>,
    pub(crate) host_dbus_address: Option<String>,
    pub(crate) manifold_dbus_destination: Option<String>,
    pub(crate) manifold_dbus_address: Option<String>,
    hosted_children: Vec<HostedChild>,
    hosted_child_event_sender: channel::Sender<HostedChildEvent>,
    async_runtime: Option<tokio::runtime::Handle>,
    hosted_child_exits: u64,
    hosted_child_failures: u64,
    hosted_child_last_status: Option<String>,
    hosted_child_completions: VecDeque<(HostedChildToken, bool, String)>,
}

impl Manifold {
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
        let mut seat = seat_state.new_wl_seat(&display_handle, "manifold");
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
        Self::init_window_lifecycle_timer(event_loop);
        let loop_handle = event_loop.handle();
        let loop_signal = event_loop.get_signal();

        let dmabuf = DmabufRuntime {
            #[cfg(feature = "shm-fallback")]
            shm_formats: MANDATORY_SHM_FORMATS,
            ..DmabufRuntime::default()
        };
        let mut wasm_registry = manifold_wasm::Registry::new();
        let wasm_host_mailbox = wasm_registry
            .register_host_mailbox(256)
            .expect("register WASM host mailbox");

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
            stats: RuntimeStats::default(),
            next_window_slot: 0,
            last_window_count: 0,
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
            session_init_actor: None,
            realm_inits: BTreeMap::new(),
            configured_workspace_plugins: BTreeMap::new(),
            configured_plugin_lifecycles: BTreeMap::new(),
            configured_plugin_resources: BTreeMap::new(),
            configured_plugin_runtime_io: BTreeMap::new(),
            filesystem_watcher_service: manifold_wasm::NativeFilesystemWatcherService::new(),
            test_surface_markers: false,
            test_harness: TestHarnessService::default(),
            manifold_session: None,
            host_dbus_address: std::env::var(MANIFOLD_HOST_DBUS_ADDRESS_ENV)
                .ok()
                .or_else(|| std::env::var("DBUS_SESSION_BUS_ADDRESS").ok()),
            manifold_dbus_destination: None,
            manifold_dbus_address: None,
            hosted_children: Vec::new(),
            hosted_child_event_sender,
            async_runtime: None,
            hosted_child_exits: 0,
            hosted_child_failures: 0,
            hosted_child_last_status: None,
            hosted_child_completions: VecDeque::new(),
        }
    }

    pub fn set_async_runtime(&mut self, runtime: tokio::runtime::Handle) {
        if self.plugin_runtime.is_none() {
            self.plugin_runtime = Some(PluginRuntime::start_builtin(
                &runtime,
                self.plugin_host_event_sender.clone(),
            ));
            self.plugin_actor_last_status = Some("builtin:started".to_string());
        }
        self.async_runtime = Some(runtime);
    }

    pub fn send_plugin_actor_message(
        &mut self,
        actor_id: &str,
        sender: String,
        body: String,
    ) -> Result<()> {
        self.drain_hosted_actor_events();
        if let Some(actor) = self.wasm_io_actors.get(actor_id) {
            actor.try_send_message(sender, body.into_bytes())?;
            self.plugin_actor_messages += 1;
            self.plugin_actor_last_status = Some(format!("{actor_id}:message_queued"));
            return Ok(());
        }
        if self
            .legacy_wasm_actor_generation
            .as_ref()
            .is_some_and(|generation| generation.contains_actor(actor_id))
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
        session: SurfaceSessionId,
        sender: &str,
        actor: &str,
        body: &[u8],
    ) -> Result<()> {
        ensure!(
            session == self.sessions.active(),
            "surface session {} is not attached to the current presentation",
            session.raw()
        );
        let fast_path = self
            .dmabuf
            .fast_path
            .as_mut()
            .context("no presentation fast path is attached")?;
        fast_path.presentation_message_sequence =
            fast_path.presentation_message_sequence.wrapping_add(1);
        let record = manifold_ipc::ActorMessageRecord::new(
            fast_path.generation,
            fast_path.presentation_message_sequence,
            session.raw(),
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
            surface_session = session.raw(),
            sender, actor, "queued session-to-presentation actor message"
        );
        Ok(())
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
                manifold_wasm::HostedWasmActor::Component(actor) => {
                    let stats = actor.mailbox_stats();
                    (
                        stats.enqueued_messages,
                        stats.dropped_messages,
                        stats.capacity,
                    )
                }
                manifold_wasm::HostedWasmActor::Preview1(actor) => {
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

    pub fn plugin_mailbox_summaries(
        &mut self,
        actor_id: &str,
    ) -> Result<Vec<PluginMailboxSummary>> {
        self.drain_hosted_actor_events();
        if let Some(actor) = self.wasm_io_actors.get(actor_id) {
            let (enqueued_messages, dropped_messages, capacity) = match actor {
                manifold_wasm::HostedWasmActor::Component(actor) => {
                    let stats = actor.mailbox_stats();
                    (
                        stats.enqueued_messages,
                        stats.dropped_messages,
                        stats.capacity,
                    )
                }
                manifold_wasm::HostedWasmActor::Preview1(actor) => {
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

    pub fn preview1_compat_stats(&self) -> Option<manifold_wasm::Preview1CompatStats> {
        manifold_wasm::aggregate_preview1_compat_stats(self.wasm_io_actors.values().filter_map(
            |actor| match actor {
                manifold_wasm::HostedWasmActor::Preview1(actor) => Some(actor.stats()),
                manifold_wasm::HostedWasmActor::Component(_) => None,
            },
        ))
    }

    pub fn filesystem_watcher_stats(&self) -> manifold_wasm::NativeFilesystemWatcherStats {
        self.filesystem_watcher_service.stats()
    }

    pub fn hosted_filesystem_watch_stats(&self) -> manifold_wasm::FilesystemWatchStats {
        self.wasm_io_actors.values().fold(
            manifold_wasm::FilesystemWatchStats::default(),
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

    /// Route a byte-oriented policy message through the shared public actor
    /// namespace. Hosted Component/Preview actors carry sender metadata in
    /// their mailbox ABI; legacy actors retain their older byte-only mailbox.
    fn try_route_wasm_actor_message(
        &self,
        actor_id: &str,
        sender: String,
        payload: Vec<u8>,
    ) -> Result<()> {
        if let Some(actor) = self.wasm_io_actors.get(actor_id) {
            return actor.try_send_message(sender, payload);
        }
        self.block_on_wasm_registry(|registry| {
            registry.try_post_named_message(actor_id, payload)
        })??;
        Ok(())
    }

    fn emit_wasm_actor_event(&mut self, event: manifold_wasm::WasmActorEventKind, payload: &str) {
        let actor_names = self
            .legacy_wasm_actor_generation
            .as_ref()
            .map_or_else(Vec::new, |generation| generation.subscribers(event));
        for actor in actor_names {
            let result = self.block_on_wasm_registry(|registry| {
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

    fn block_on_wasm_registry<T>(
        &self,
        f: impl FnOnce(&mut manifold_wasm::Registry) -> T,
    ) -> Result<T> {
        let registry = Arc::clone(&self.wasm_registry);
        let runtime = self
            .async_runtime
            .as_ref()
            .context("async runtime is not initialized for WASM registry use")?;
        Ok(runtime.block_on(async move {
            let mut registry = registry.lock().await;
            f(&mut registry)
        }))
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

    fn drain_hosted_actor_events(&mut self) {
        enum InitRequest {
            StartActor(SurfaceSessionId, String, String, Vec<String>),
            StopActor(SurfaceSessionId, String),
            StartApp(SurfaceSessionId, String, Vec<String>),
        }

        let mut events = Vec::new();
        let mut init_requests = Vec::new();
        let mut local_messages = Vec::new();
        let mut presentation_messages = Vec::new();
        for (actor_name, actor) in &mut self.wasm_io_actors {
            match actor {
                manifold_wasm::HostedWasmActor::Component(actor) => {
                    while let Some(event) = actor.try_recv_event() {
                        events.push((actor_name.clone(), Ok(event)));
                    }
                }
                manifold_wasm::HostedWasmActor::Preview1(actor) => {
                    if let Some(message) = actor.try_take_failure() {
                        events.push((actor_name.clone(), Err(message)));
                    }
                }
            }
        }

        for (actor_name, event) in events {
            let event = match event {
                Ok(event) => event,
                Err(message) => {
                    self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
                    self.plugin_actor_last_status = Some(format!("{actor_name}:dispatcher_failed"));
                    warn!(actor = %actor_name, error = %message, "Preview 1 actor dispatcher failed");
                    if let Some((session, child_id)) = self.realm_init_child_owner(&actor_name) {
                        self.send_realm_init_child_event(
                            session,
                            manifold_wasm::ComponentChildEvent::Failed {
                                id: child_id,
                                message,
                            },
                        );
                    }
                    continue;
                }
            };
            match event {
                manifold_wasm::ComponentActorEvent::DispatcherFailed { message } => {
                    self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
                    self.plugin_actor_last_status = Some(format!("{actor_name}:dispatcher_failed"));
                    warn!(actor = %actor_name, error = %message, "component actor dispatcher failed");
                    if let Some((session, child_id)) = self.realm_init_child_owner(&actor_name) {
                        self.send_realm_init_child_event(
                            session,
                            manifold_wasm::ComponentChildEvent::Failed {
                                id: child_id,
                                message,
                            },
                        );
                    }
                }
                manifold_wasm::ComponentActorEvent::WorkerFinished { worker, .. } => {
                    self.plugin_actor_last_status =
                        Some(format!("{actor_name}:worker_{}_finished", worker.raw()));
                    info!(
                        actor = %actor_name,
                        worker = worker.raw(),
                        "component actor worker finished"
                    );
                }
                manifold_wasm::ComponentActorEvent::WorkerFailed { worker, message } => {
                    self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
                    self.plugin_actor_last_status =
                        Some(format!("{actor_name}:worker_{}_failed", worker.raw()));
                    warn!(actor = %actor_name, worker = worker.raw(), error = %message, "component actor worker failed");
                }
                manifold_wasm::ComponentActorEvent::WorkerCancelled { worker } => {
                    self.plugin_actor_last_status =
                        Some(format!("{actor_name}:worker_{}_cancelled", worker.raw()));
                    info!(
                        actor = %actor_name,
                        worker = worker.raw(),
                        "component actor worker cancelled"
                    );
                }
                manifold_wasm::ComponentActorEvent::Stopped => {
                    self.plugin_actor_last_status = Some(format!("{actor_name}:stopped"));
                    info!(actor = %actor_name, "component actor stopped");
                    if let Some((session, child_id)) = self.realm_init_child_owner(&actor_name) {
                        self.send_realm_init_child_event(
                            session,
                            manifold_wasm::ComponentChildEvent::Stopped { id: child_id },
                        );
                    }
                }
                manifold_wasm::ComponentActorEvent::ActorStartRequested {
                    id,
                    path,
                    capabilities,
                } => {
                    if let Some(session) = self.realm_init_root_session(&actor_name) {
                        init_requests.push(InitRequest::StartActor(
                            session,
                            id,
                            path,
                            capabilities,
                        ));
                    }
                }
                manifold_wasm::ComponentActorEvent::ActorStopRequested { id } => {
                    if let Some(session) = self.realm_init_root_session(&actor_name) {
                        init_requests.push(InitRequest::StopActor(session, id));
                    }
                }
                manifold_wasm::ComponentActorEvent::AppStartRequested { id, argv } => {
                    if let Some(session) = self.realm_init_root_session(&actor_name) {
                        init_requests.push(InitRequest::StartApp(session, id, argv));
                    }
                }
                manifold_wasm::ComponentActorEvent::LocalMessageRequested { actor, body } => {
                    local_messages.push((actor_name, actor, body));
                }
                manifold_wasm::ComponentActorEvent::PresentationMessageRequested {
                    actor,
                    body,
                } => {
                    presentation_messages.push((actor_name, actor, body));
                }
                _ => {}
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
            let owner = self
                .realm_init_root_session(&sender_actor)
                .map(|session| (session, "init".to_string()))
                .or_else(|| self.realm_init_child_owner(&sender_actor));
            let Some((session, sender)) = owner else {
                warn!(actor = %sender_actor, "rejected presentation message from actor outside a session realm");
                continue;
            };
            if let Err(error) =
                self.send_presentation_actor_message(session, &sender, &recipient, &body)
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
            "manifold-session".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Manifold".into(),
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
                let result = state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()));
                if let Err(err) = result {
                    state.stats.wayland_client_insert_errors += 1;
                    warn!(?err, "insert Wayland client failed");
                    return;
                }
                if let Err(err) = state.display_handle.flush_clients() {
                    warn!(?err, "flush clients after insert failed");
                }
            })
            .expect("insert wayland socket");

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    dispatch_wayland_clients(display, state);
                    if let Err(err) = state.display_handle.flush_clients() {
                        warn!(?err, "flush clients after dispatch failed");
                    }
                    Ok(PostAction::Continue)
                },
            )
            .expect("insert wayland display");

        socket_name
    }

    fn init_window_lifecycle_timer(event_loop: &mut EventLoop<'static, Self>) {
        event_loop
            .handle()
            .insert_source(
                Timer::from_duration(WINDOW_LIFECYCLE_REFRESH_INTERVAL),
                |_, (), state| {
                    state.refresh_hosted_children();
                    state.refresh_window_state_from_lifecycle_timer();
                    TimeoutAction::ToDuration(WINDOW_LIFECYCLE_REFRESH_INTERVAL)
                },
            )
            .expect("insert window lifecycle timer");
    }

    fn init_hosted_child_event_source(
        event_loop: &mut EventLoop<'static, Self>,
    ) -> channel::Sender<HostedChildEvent> {
        let (sender, receiver) = channel::channel();
        event_loop
            .handle()
            .insert_source(receiver, |event, (), state| {
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
                let channel::Event::Msg(event) = event else {
                    return;
                };
                state.handle_host_clipboard_event(event);
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
                let channel::Event::Msg(event) = event else {
                    return;
                };
                state.handle_plugin_host_event(event);
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
        command_builder.env(MANIFOLD_PANE_ID_ENV, pane_id.to_string());
        command_builder.env(MANIFOLD_SURFACE_SESSION_ID_ENV, session.raw().to_string());
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
        command_builder.env(MANIFOLD_PANE_ID_ENV, pane_id.to_string());
        command_builder.env(MANIFOLD_SURFACE_SESSION_ID_ENV, session.raw().to_string());
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
        if let Some(session) = self.manifold_session.as_deref() {
            child.env(MANIFOLD_SESSION_ENV, session);
        }
        if let Some(destination) = self.manifold_dbus_destination.as_deref() {
            child.env(MANIFOLDD_DBUS_DESTINATION_ENV, destination);
        }
        if let Some(address) = self.manifold_dbus_address.clone() {
            child.env(MANIFOLDD_DBUS_ADDRESS_ENV, &address);
            match self.ensure_session_dbus_bus(session) {
                Ok(session_bus_address) => {
                    child.env("DBUS_SESSION_BUS_ADDRESS", session_bus_address);
                    child.env(MANIFOLD_SURFACE_SESSION_ID_ENV, session.raw().to_string());
                }
                Err(err) => {
                    warn!(
                        ?err,
                        session = session.raw(),
                        "failed to start session DBus bus; managed client inherits Manifold control bus"
                    );
                    child.env("DBUS_SESSION_BUS_ADDRESS", &address);
                }
            }
        } else {
            warn!(
                "managed client inherits host DBUS_SESSION_BUS_ADDRESS; DBus policy modes off/proxy/private are not enforced yet"
            );
        }
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
    ) {
        self.configure_presentation_output_inner(width, height, refresh_millihz, false);
    }

    fn configure_presentation_output_inner(
        &mut self,
        width: i32,
        height: i32,
        refresh_millihz: i32,
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
        self.session_output
            .change_current_state(Some(mode), None, None, None);
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
    ) -> Option<&manifold_wasm::TestHarnessCleanupResult> {
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
            manifold_wasm::WasmActorEventKind::PaneMapped,
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
                let pane_id = process_tree_env_var(surface_pid, MANIFOLD_PANE_ID_ENV)?
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
        process_tree_env_var(pid, MANIFOLD_SURFACE_SESSION_ID_ENV)
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
        self.prune_empty_unfocused_surface_sessions();
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
        self.sessions
            .contains(session)
            .then(|| SurfaceSessionSummary {
                id: session.raw(),
                name: self.surface_session_name(session).to_string(),
                active: session == self.sessions.active(),
                windows: self.surface_session_window_count(session),
                working_directory: self
                    .sessions
                    .working_directory(session)
                    .map(|directory| directory.to_string_lossy().into_owned()),
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

    fn prune_empty_unfocused_surface_sessions(&mut self) -> bool {
        let sessions = self
            .sessions
            .sessions()
            .iter()
            .copied()
            .filter(|session| *session != self.sessions.active())
            .filter(|session| !self.surface_session_has_content_panes_or_pending(*session))
            .collect::<Vec<_>>();
        if sessions.is_empty() {
            return false;
        }
        for session in sessions {
            self.remove_surface_session_record(session);
        }
        true
    }

    fn remove_surface_session_record(&mut self, session: SurfaceSessionId) {
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

    fn surface_session_has_content_panes_or_pending(&self, session: SurfaceSessionId) -> bool {
        self.sessions
            .surface_assignments()
            .iter()
            .any(|(surface, assigned_session)| {
                *assigned_session == session
                    && self.pane_for_toplevel_surface(surface).is_some()
                    && self.window_for_any_toplevel_surface(surface).is_some()
            })
            || self
                .pending_child_panes
                .iter()
                .any(|pending| pending.session == session)
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
        self.prune_empty_unfocused_surface_sessions();
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
        self.prune_empty_unfocused_surface_sessions();
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
        self.prune_empty_unfocused_surface_sessions();
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
        self.configure_presentation_client_toplevel_size(pane, &surface, toplevel, requested);
        true
    }

    pub(crate) fn renegotiate_presentation_client_pane_size(
        &mut self,
        surface: &WlSurface,
    ) -> bool {
        let Some(pane) = self.pane_for_toplevel_surface(surface) else {
            return false;
        };
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
                    current.size == Some(size) && current.bounds == Some(size)
                })
        });
        if already_configured {
            return false;
        }
        toplevel.with_pending_state(|state| {
            state.size = Some(size);
            state.bounds = Some(size);
        });
        let configured = toplevel.send_pending_configure().is_some();
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
            .try_send(manifold_ipc::CriticalRecord {
                opcode: manifold_ipc::FRAME_FOCUS_PRESENTATION,
                connection_generation: fast_path.generation,
                ..manifold_ipc::CriticalRecord::default()
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
            .try_send(manifold_ipc::CriticalRecord {
                opcode: manifold_ipc::FRAME_CLOSE_PRESENTATION,
                connection_generation: fast_path.generation,
                ..manifold_ipc::CriticalRecord::default()
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
        if publisher != MANIFOLD_STATUS_PUBLISHER {
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

    /// Configure the root artifact used for every new surface-session realm.
    pub(crate) fn configure_session_init_actor(&mut self, path: &Path) -> Result<()> {
        ensure!(
            self.session_init_actor.is_none(),
            "session init actor is already configured"
        );
        let path = path
            .canonicalize()
            .with_context(|| format!("resolve session init actor {}", path.display()))?;
        let artifact = manifold_wasm::WasmPluginArtifact::load(&path)?;
        ensure!(
            matches!(
                artifact,
                manifold_wasm::WasmPluginArtifact::Component { .. }
            ),
            "realm init actor must be a Component Model actor"
        );
        self.session_init_actor = Some(path);
        let sessions = self.sessions.sessions().to_vec();
        for session in sessions {
            self.start_session_init_actor(session)?;
        }
        Ok(())
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
            Some(&(lifecycle.clone(), manifold_wasm::PluginStartReason::Initial)),
            ConfiguredPluginResources::default(),
            manifold_wasm::RuntimeIoCapabilities::default(),
            true,
            true,
            Some(session),
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
        let Some(init) = self.realm_inits.remove(&session) else {
            return Ok(());
        };
        let mut first_error = None;
        for actor_id in init.children.values() {
            if let Err(error) = self.stop_hosted_io_actor(actor_id)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Err(error) = self.stop_hosted_io_actor_lifecycle(
            &init.actor_id,
            manifold_wasm::PluginStopReason::Shutdown,
            None,
        ) && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = self.stop_hosted_io_actor(&init.actor_id)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        self.configured_plugin_lifecycles.remove(&init.actor_id);
        first_error.map_or(Ok(()), Err)
    }

    /// Service lifecycle requests and failure feedback for the session init
    /// actor. This is safe to call on every compositor turn.
    pub(crate) fn service_realm_init_actor(&mut self) {
        self.flush_presentation_actor_messages();
        self.drain_hosted_actor_events();
        let completed = self
            .realm_inits
            .iter()
            .flat_map(|(session, init)| {
                init.apps.iter().filter_map(|(id, token)| {
                    let event = match self.hosted_child_status(*token) {
                        HostedChildStatus::Exited { success: true, .. } => {
                            manifold_wasm::ComponentAppEvent::Stopped { id: id.clone() }
                        }
                        HostedChildStatus::Exited {
                            success: false,
                            detail,
                        } => manifold_wasm::ComponentAppEvent::Failed {
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

    fn handle_realm_init_start(
        &mut self,
        session: SurfaceSessionId,
        id: String,
        guest_path: &str,
        capabilities: &[String],
    ) {
        let result = self.start_realm_init_child(session, &id, guest_path, capabilities);
        match result {
            Ok(generation) => {
                self.send_realm_init_child_event(
                    session,
                    manifold_wasm::ComponentChildEvent::Started { id, generation },
                );
            }
            Err(error) => {
                self.plugin_actor_failures = self.plugin_actor_failures.saturating_add(1);
                warn!(actor = %id, path = %guest_path, error = %error, "realm init failed to start child actor");
                self.send_realm_init_child_event(
                    session,
                    manifold_wasm::ComponentChildEvent::Failed {
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
    ) -> Result<u64> {
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
                || self
                    .realm_inits
                    .get(&session)
                    .is_some_and(|init| init.children.get(id) == Some(&actor_id)),
            "init actor cannot replace an actor it does not own"
        );
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
        let path = root.join(relative).canonicalize().with_context(|| {
            format!("resolve init child actor {}", root.join(relative).display())
        })?;
        ensure!(
            path.starts_with(&canonical_root),
            "init child actor path must stay inside the init package root"
        );
        ensure!(
            capabilities.iter().all(|capability| {
                matches!(
                    capability.as_str(),
                    "filesystem:workspace-read"
                        | "filesystem:workspace-write"
                        | "pty"
                        | "network:loopback"
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
        self.replace_wasm_component_actor(
            &actor_id,
            &path,
            filesystem.as_ref(),
            None,
            ConfiguredPluginResources::default(),
            runtime_io,
            false,
            false,
            Some(session),
        )?;
        let generation = self
            .wasm_io_actors
            .get(&actor_id)
            .context("init child did not produce a hosted actor")?
            .generation();
        if let Some(init) = self.realm_inits.get_mut(&session) {
            init.children.insert(id.to_string(), actor_id);
        }
        Ok(generation)
    }

    fn handle_realm_init_stop(&mut self, session: SurfaceSessionId, id: String) {
        let owned = self
            .realm_inits
            .get(&session)
            .and_then(|init| init.children.get(&id))
            .cloned();
        let Some(actor_id) = owned else {
            self.send_realm_init_child_event(
                session,
                manifold_wasm::ComponentChildEvent::Failed {
                    id,
                    message: "init actor may stop only its own children".to_string(),
                },
            );
            return;
        };
        let result = self.stop_hosted_io_actor(&actor_id);
        if let Some(init) = self.realm_inits.get_mut(&session) {
            init.children.remove(&id);
        }
        match result {
            Ok(()) => {
                self.send_realm_init_child_event(
                    session,
                    manifold_wasm::ComponentChildEvent::Stopped { id },
                );
            }
            Err(error) => {
                self.send_realm_init_child_event(
                    session,
                    manifold_wasm::ComponentChildEvent::Failed {
                        id,
                        message: format!("{error:#}"),
                    },
                );
            }
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
                    manifold_wasm::ComponentAppEvent::Started {
                        id,
                        pane: token.raw(),
                    },
                );
            }
            Err(error) => self.send_realm_init_app_event(
                session,
                manifold_wasm::ComponentAppEvent::Failed {
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
        event: manifold_wasm::ComponentChildEvent,
    ) {
        let Some(init) = self.realm_inits.get(&session) else {
            return;
        };
        let Some(manifold_wasm::HostedWasmActor::Component(actor)) =
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
        event: manifold_wasm::ComponentAppEvent,
    ) {
        let Some(init) = self.realm_inits.get(&session) else {
            return;
        };
        let Some(manifold_wasm::HostedWasmActor::Component(actor)) =
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
            manifold_wasm::RuntimeIoCapabilities::default(),
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
            lifecycle.map(|config| (config, manifold_wasm::PluginStartReason::Replacement)),
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
        runtime_io: manifold_wasm::RuntimeIoCapabilities,
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
            manifold_wasm::PluginStartReason::Replacement
        } else {
            manifold_wasm::PluginStartReason::Initial
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
        if let Some(writable) = workspace_filesystem_writable
            && self.wasm_io_actors.contains_key(module_id)
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
        if self.wasm_io_actors.contains_key(module_id) {
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
            self.send_plugin_actor_message(module_id, "host".to_string(), message)?;
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
            let lifecycle_result = self.stop_hosted_io_actor_lifecycle(
                &actor_name,
                manifold_wasm::PluginStopReason::SessionRebind,
                None,
            );
            let hard_result = self.stop_hosted_io_actor(&actor_name);
            let result = lifecycle_result.and(hard_result);
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
                        .map(|config| (config, manifold_wasm::PluginStartReason::SessionRebind)),
                    plugin.resources,
                    plugin.runtime_io,
                )
                .and_then(|()| {
                    if let Some(message) = plugin.startup_message {
                        self.send_plugin_actor_message(&actor_name, "host".to_string(), message)?;
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
        actor: &manifold_wasm::HostedWasmActor,
        lifecycle: &ConfiguredPluginLifecycle,
        reason: manifold_wasm::PluginStartReason,
        session: Option<SurfaceSessionId>,
    ) -> Result<()> {
        let runtime = self
            .async_runtime
            .clone()
            .context("async runtime is not initialized")?;
        let context = manifold_wasm::PluginStartContext {
            instance_id: actor_name.to_string(),
            generation: actor.generation(),
            reason,
            session_id: Some(session.unwrap_or_else(|| self.sessions.active()).raw()),
            workspace_root: ".".to_string(),
            data: lifecycle
                .data
                .iter()
                .map(|(key, value)| manifold_wasm::PluginLifecycleEntry {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
        };
        runtime
            .block_on(actor.lifecycle(
                manifold_wasm::PluginLifecycleEvent::Start(context),
                lifecycle.start_timeout,
            ))
            .context("plugin start lifecycle failed")
    }

    fn stop_hosted_io_actor_lifecycle(
        &mut self,
        actor_name: &str,
        reason: manifold_wasm::PluginStopReason,
        next_generation: Option<u64>,
    ) -> Result<()> {
        let Some(lifecycle) = self.configured_plugin_lifecycles.get(actor_name).cloned() else {
            return Ok(());
        };
        let Some(actor) = self.wasm_io_actors.get(actor_name) else {
            return Ok(());
        };
        let runtime = self
            .async_runtime
            .clone()
            .context("async runtime is not initialized")?;
        let context = manifold_wasm::PluginStopContext {
            instance_id: actor_name.to_string(),
            generation: actor.generation(),
            reason,
            next_generation,
        };
        runtime
            .block_on(actor.lifecycle(
                manifold_wasm::PluginLifecycleEvent::Stop(context),
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
        lifecycle: Option<(ConfiguredPluginLifecycle, manifold_wasm::PluginStartReason)>,
        resources: ConfiguredPluginResources,
        runtime_io: manifold_wasm::RuntimeIoCapabilities,
    ) -> Result<()> {
        let artifact = manifold_wasm::WasmPluginArtifact::load(path)?;
        let inventory = match artifact {
            manifold_wasm::WasmPluginArtifact::Component { core_module_error } => {
                let instance_id =
                    instance_id.map_or_else(|| wasm_plugin_instance_id(path), str::to_string);
                return self
                    .replace_wasm_component_actor(&instance_id, path, filesystem.as_ref(), lifecycle.as_ref(), resources, runtime_io, false, false, None)
                    .with_context(|| {
                        format!(
                            "{} is neither a legacy Manifold module ({core_module_error}) nor a component actor",
                            path.display()
                        )
                });
            }
            manifold_wasm::WasmPluginArtifact::Preview1 => {
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
            manifold_wasm::WasmPluginArtifact::Legacy(artifact) => artifact.inventory,
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
        lifecycle: Option<&(ConfiguredPluginLifecycle, manifold_wasm::PluginStartReason)>,
        resources: ConfiguredPluginResources,
        runtime_io: manifold_wasm::RuntimeIoCapabilities,
        actor_management: bool,
        app_management: bool,
        lifecycle_session: Option<SurfaceSessionId>,
    ) -> Result<()> {
        let Some(runtime) = self.async_runtime.clone() else {
            anyhow::bail!("async runtime is not initialized");
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
        let actor = runtime.block_on(manifold_wasm::ComponentActor::load(
            path,
            manifold_wasm::ComponentActorConfig {
                // Actors without an explicit workspace grant receive the
                // narrowest useful default: their package directory read-only.
                filesystem: Some(manifold_wasm::FilesystemGrant {
                    host_path,
                    guest_path: ".".to_string(),
                    writable,
                }),
                filesystem_watcher_service: self.filesystem_watcher_service.clone(),
                max_filesystem_watches: resources.filesystem_watches,
                max_filesystem_watch_events: resources.filesystem_watch_events,
                runtime_io: manifold_wasm::RuntimeIoConfig {
                    capabilities: runtime_io,
                    working_directory: filesystem.map(|grant| grant.host_path.clone()),
                    ..manifold_wasm::RuntimeIoConfig::default()
                },
                actor_management,
                app_management,
                presentation_messaging: lifecycle_session.is_some(),
                ..manifold_wasm::ComponentActorConfig::default()
            },
        ))?;
        let mut candidate = manifold_wasm::HostedWasmActor::Component(actor);
        let next_generation = candidate.generation();
        if let Err(error) = self.stop_hosted_io_actor_lifecycle(
            actor_name,
            manifold_wasm::PluginStopReason::Replacement,
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
        self.plugin_actor_last_status = Some(format!("{actor_name}:component_started"));
        info!(actor = %actor_name, path = %path.display(), "started component plugin actor");
        Ok(())
    }

    fn replace_wasm_preview1_actor(
        &mut self,
        actor_name: &str,
        path: &Path,
        filesystem: Option<&HostedPluginFilesystem>,
        lifecycle: Option<&(ConfiguredPluginLifecycle, manifold_wasm::PluginStartReason)>,
        resources: ConfiguredPluginResources,
        runtime_io: manifold_wasm::RuntimeIoCapabilities,
    ) -> Result<()> {
        let Some(runtime) = self.async_runtime.clone() else {
            anyhow::bail!("async runtime is not initialized");
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
        let actor = runtime.block_on(
            manifold_wasm::Preview1CompatActor::load_with_watcher_service(
                path,
                manifold_wasm::Preview1ActorCapabilities {
                    filesystem: Some(manifold_wasm::Preview1FilesystemGrant {
                        host_path,
                        guest_path: ".".to_string(),
                        writable,
                    }),
                    runtime_io,
                },
                manifold_wasm::Preview1WorkerConfig {
                    max_filesystem_watches: resources.filesystem_watches,
                    max_filesystem_watch_events: resources.filesystem_watch_events,
                    ..manifold_wasm::Preview1WorkerConfig::default()
                },
                self.filesystem_watcher_service.clone(),
            ),
        )?;
        let mut candidate = manifold_wasm::HostedWasmActor::Preview1(actor);
        let next_generation = candidate.generation();
        if let Err(error) = self.stop_hosted_io_actor_lifecycle(
            actor_name,
            manifold_wasm::PluginStopReason::Replacement,
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
        actors: &[manifold_wasm::WasmActorDeclaration],
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
        self.drain_hosted_actor_events();
        let Some(mut actor) = self.wasm_io_actors.remove(actor_name) else {
            return Ok(());
        };
        let Some(runtime) = self.async_runtime.as_ref() else {
            anyhow::bail!("cannot join hosted I/O actor without the async runtime");
        };
        runtime.block_on(async {
            let result = match &mut actor {
                manifold_wasm::HostedWasmActor::Component(actor) => actor.shutdown().await,
                manifold_wasm::HostedWasmActor::Preview1(actor) => {
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
        let actor_names = self.wasm_io_actors.keys().cloned().collect::<Vec<_>>();
        let mut first_error = None;
        for actor_name in actor_names {
            if let Err(error) = self.stop_hosted_io_actor_lifecycle(
                &actor_name,
                manifold_wasm::PluginStopReason::Shutdown,
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
        let Some(generation) = self.legacy_wasm_actor_generation.take() else {
            return Ok(());
        };
        let actor_ids = generation.actor_ids().clone();
        let Some(runtime) = self.async_runtime.as_ref() else {
            anyhow::bail!("cannot cancel and join legacy WASM actors without the async runtime");
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
        actors: &[manifold_wasm::WasmActorDeclaration],
    ) -> Result<()> {
        let Some(runtime) = self.async_runtime.clone() else {
            anyhow::bail!("async runtime is not initialized");
        };
        let registry = Arc::clone(&self.wasm_registry);
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
                .block_on(manifold_wasm::WasmActor::load_named(
                    path,
                    mailbox,
                    &actor.export_name,
                ))
                .with_context(|| format!("load candidate WASM actor {:?}", actor.name))?;
            actor_ids.insert(actor.name.clone(), candidate.actor_id());
            registrations.push(registration);
            candidates.push((actor.clone(), candidate));
        }

        let old_names = self.legacy_wasm_actor_generation.as_ref().map_or_else(
            Vec::new,
            manifold_wasm::LegacyWasmActorGeneration::actor_names,
        );
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
        self.legacy_wasm_actor_generation = Some(manifold_wasm::LegacyWasmActorGeneration::start(
            &runtime,
            candidates,
            actor_ids,
            Arc::clone(&registry),
            path,
            move |event| {
                let manifold_wasm::LegacyWasmRuntimeEvent::DrainHostMailbox = event;
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
            !manifold_owned_shortcut(&shortcut),
            "shortcut conflicts with an existing Manifold-owned shortcut"
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

    pub fn refresh_window_state_from_lifecycle_timer(&mut self) {
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
        self.prune_empty_unfocused_surface_sessions();
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
    display: &mut calloop::generic::NoIoDrop<Display<Manifold>>,
    state: &mut Manifold,
) {
    // SAFETY: the callback runs while calloop exclusively owns this source;
    // `dispatch_clients` preserves the display and its registered poll fd.
    if let Err(err) = unsafe { display.get_mut().dispatch_clients(state) } {
        state.stats.wayland_dispatch_errors += 1;
        warn!(?err, "dispatch Wayland clients failed");
    }
}

fn default_surface_session_name(session: SurfaceSessionId) -> String {
    let n = session.raw().saturating_sub(1) as usize;
    let adjective = SESSION_NAME_ADJECTIVES
        .get(n % SESSION_NAME_ADJECTIVES.len())
        .copied()
        .unwrap_or("quiet");
    let noun = SESSION_NAME_NOUNS
        .get((n / SESSION_NAME_ADJECTIVES.len()) % SESSION_NAME_NOUNS.len())
        .copied()
        .unwrap_or("workspace");
    let action = SESSION_NAME_ACTIONS
        .get(
            (n / (SESSION_NAME_ADJECTIVES.len() * SESSION_NAME_NOUNS.len()))
                % SESSION_NAME_ACTIONS.len(),
        )
        .copied()
        .unwrap_or("working");
    format!("{adjective}-{noun}-{action}")
}

fn session_init_actor_id(session: SurfaceSessionId) -> String {
    format!("{SESSION_INIT_ACTOR_PREFIX}.{}", session.raw())
}

fn session_init_child_actor_id(session: SurfaceSessionId, child: &str) -> String {
    format!("manifold.session.{}.{}", session.raw(), child)
}

fn fallback_marker_id(index: usize) -> u64 {
    1_000_000 + u64::try_from(index).unwrap_or(u64::MAX - 1_000_000)
}

fn current_process_directory() -> PathBuf {
    let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    directory.canonicalize().unwrap_or(directory)
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

fn toplevel_pid(state: &Manifold, surface: &WlSurface) -> Option<String> {
    surface
        .client()
        .and_then(|client| client.get_credentials(&state.display_handle).ok())
        .map(|credentials| credentials.pid.to_string())
}

fn toplevel_matches_identity(
    state: &Manifold,
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
    state: &Manifold,
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

fn manifold_owned_shortcut(shortcut: &str) -> bool {
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
    let message: manifold_wasm::HostMessage =
        serde_json::from_str(body).context("decode plugin host message")?;
    Ok(match message {
        manifold_wasm::HostMessage::FocusPane { pane } => WorkspaceCommand::FocusPane(pane),
        manifold_wasm::HostMessage::SpawnApp { argv } => WorkspaceCommand::SpawnApp(argv),
        manifold_wasm::HostMessage::FocusOrSpawnApp { app_id, argv } => {
            WorkspaceCommand::FocusOrSpawnApp { app_id, argv }
        }
        manifold_wasm::HostMessage::SwitchActiveSessionNext => {
            WorkspaceCommand::Surface(SurfaceCommand::SwitchActiveSessionNext)
        }
        manifold_wasm::HostMessage::CreateAndAttachSession => {
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
    pub input: manifold_ipc::CriticalConsumer,
    pub frames: manifold_ipc::CriticalProducer,
    pub catalog: manifold_ipc::CriticalProducer,
    pub actor_messages: manifold_ipc::ActorMessageConsumer,
    pub presentation_messages: manifold_ipc::ActorMessageProducer,
    pub frame_notifications: manifold_ipc::EventNotifier,
    pub catalog_notifications: manifold_ipc::EventNotifier,
    pub catalog_revision: u64,
    pub last_catalog_payload: Vec<u8>,
    pub presentation_message_notifications: manifold_ipc::EventNotifier,
    pub presentation_message_sequence: u64,
    pub deferred_presentation_messages: VecDeque<Box<manifold_ipc::ActorMessageRecord>>,
    pub resources: HashMap<WeakDmabuf, u64>,
    pub buffers: HashMap<u64, Dmabuf>,
    pub resource_app_ids: HashMap<u64, String>,
    pub window_buffers: HashMap<u64, Dmabuf>,
    pub next_resource_id: u64,
    pub last_presented_sequence: Option<u64>,
    pub in_flight_sequence: Option<u64>,
    pub pending_redraw: bool,
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
            "Wayland client disconnected from Manifold"
        );
    }
}
