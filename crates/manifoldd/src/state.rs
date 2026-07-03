#![allow(
    clippy::allow_attributes_without_reason,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::filter_map_next,
    clippy::indexing_slicing,
    clippy::iter_on_single_items,
    clippy::let_underscore_must_use,
    clippy::match_same_arms,
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::option_if_let_else,
    clippy::question_mark,
    clippy::struct_field_names,
    clippy::too_many_arguments,
    clippy::undocumented_unsafe_blocks,
    clippy::unnecessary_debug_formatting,
    clippy::unused_self,
    clippy::unwrap_used,
    clippy::useless_let_if_seq,
    reason = "tracked in issues.md: central runtime state currently owns too many invariants and conversions directly"
)]
// TODO(lint-structure): decompose Manifold state into typed geometry, session, focus, and registry services.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
#[cfg(feature = "shm-fallback")]
use smithay::wayland::shm::ShmState;
use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState, keyboard::XkbConfig},
    output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel},
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
    utils::{Logical, Point, SERIAL_COUNTER, Transform},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        dmabuf::{DmabufFeedback, DmabufGlobal, DmabufState},
        drm_syncobj::DrmSyncobjState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::{XdgShellState, XdgToplevelSurfaceData, decoration::XdgDecorationState},
        socket::ListeningSocketSource,
    },
};
use tracing::{info, warn};

use crate::{
    MANIFOLD_HOST_DBUS_ADDRESS_ENV, MANIFOLD_PANE_ID_ENV, MANIFOLD_SESSION_ENV,
    MANIFOLDD_DBUS_ADDRESS_ENV, MANIFOLDD_DBUS_DESTINATION_ENV,
    browser::{BrowserInstanceSummary, BrowserLaunchOptions, BrowserManager},
    layout::{
        HitResult, InputBehavior, LayoutAction, LayoutAxis, LayoutItemId, LayoutItemKind,
        LayoutLeafId, LayoutLeafKind, LayoutRect, LayoutSize, LayoutState, LocalPoint,
        LocalTransform, ResolvedLayout, ResolvedLayoutItem,
    },
    plugin_runtime::{PluginActorSummary, PluginHostEvent, PluginRuntime},
    presentation::PresenterHandle,
    session_bus::{SessionBusHandle, SessionBusRealm},
    wasm_key::WasmKeyHandler,
    wasm_key::WasmKeyPane,
    wasm_map::{MapHookInput, MapHookOutcome, MapHookResponse, WasmMapHook},
    wasm_test::{
        TestHarnessCompletion, TestHarnessOutcomeKind, TestHarnessRuntime, service_request,
    },
    winit_presenter::SharedPresenterRuntime,
};

#[cfg(feature = "shm-fallback")]
const MANDATORY_SHM_FORMATS: usize = 2;
const WINDOW_LIFECYCLE_REFRESH_INTERVAL: Duration = Duration::from_millis(50);
const LEFT_PANEL_DEFAULT_WIDTH: i32 = 280;
const LEFT_PANEL_MIN_WIDTH: i32 = 180;
const LEFT_PANEL_MAX_WIDTH: i32 = 560;
const LEFT_PANEL_KEYBOARD_STEP: i32 = 32;
const LEFT_PANEL_SPLITTER_HITBOX: f64 = 8.0;
const DEFAULT_NATIVE_AUTO_SPLIT_VISIBLE_LIMIT: usize = 3;
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
pub enum PresentationStatus {
    Attached,
    Detached,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingChildPane {
    pid: Option<u32>,
    pane_id: u64,
    session: SurfaceSessionId,
    presentation: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegisteredToplevelPane {
    pane_id: u64,
    session: SurfaceSessionId,
    presentation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PanelReservation {
    surface: WlSurface,
    edge: PanelEdge,
    width: i32,
    visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelEdge {
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum InputMode {
    Passthrough,
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedLayoutSource {
    NativeFallback,
    WasmPolicy,
}

#[derive(Debug, Clone)]
struct ResolvedPresentationHit {
    layout_hit: HitResult,
    window_hit: Option<(Window, Point<i32, Logical>)>,
}

#[derive(Debug, Clone)]
struct NativeSessionLayout {
    layout: LayoutState,
    leaf_selected_panes: Vec<(LayoutLeafId, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct SurfaceSessionId(u64);

impl SurfaceSessionId {
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

pub type ExtensionMetadata = BTreeMap<String, String>;
pub const PASSTHROUGH_STATUS_CHANNEL: &str = "workspace.input.passthrough";
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
struct ExtensionRegistry {
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
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    #[allow(dead_code)]
    pub dmabuf: DmabufRuntime,
    pub popups: PopupManager,
    pub seat: Seat<Self>,

    pub presentation: PresentationStatus,
    pub presentation_started: bool,
    pub presentation_attach_failures: u64,
    pub presentation_runtime_failures: u64,
    pub presentation_last_error: Option<String>,
    presentation_window_focused: bool,
    pending_presentation_focus_activation_token: Option<String>,
    pub attach_requested: bool,
    pub attach_schedule_pending: bool,
    pub detach_requested: bool,
    pub browsers: BrowserManager,
    pub layout: LayoutState,
    accepted_resolved_layout: ResolvedLayout,
    resolved_layout_source: ResolvedLayoutSource,
    pub stats: RuntimeStats,
    next_window_slot: usize,
    last_window_count: usize,
    active_toplevel_surface: Option<WlSurface>,
    pointer_hover_toplevel_surface: Option<WlSurface>,
    pending_pointer_focus_surface: Option<WlSurface>,
    layout_leaf_selected_panes: Vec<(LayoutLeafId, u64)>,
    native_auto_split_visible_limit: usize,
    native_session_layouts: BTreeMap<SurfaceSessionId, NativeSessionLayout>,
    fullscreen_toplevels: Vec<(WlSurface, Option<wl_output::WlOutput>)>,
    next_pane_id: u64,
    window_pane_ids: Vec<(WlSurface, u64)>,
    pending_child_panes: Vec<PendingChildPane>,
    pub(crate) ctrl_pressed: bool,
    pub(crate) alt_pressed: bool,
    pub(crate) shift_pressed: bool,
    pub(crate) ctrl_pressed_key: Option<smithay::backend::input::Keycode>,
    pub(crate) alt_pressed_key: Option<smithay::backend::input::Keycode>,
    pub(crate) shift_pressed_key: Option<smithay::backend::input::Keycode>,
    pub(crate) ctrl_forwarded_to_client: bool,
    pub(crate) alt_forwarded_to_client: bool,
    pub(crate) shift_forwarded_to_client: bool,
    pub(crate) forwarded_pressed_keys: Vec<smithay::backend::input::Keycode>,
    pub(crate) suppress_mode_toggle_key_release: bool,
    pub(crate) shortcut_prefix_active: bool,
    pub(crate) suppress_shortcut_prefix_key_release: Option<smithay::backend::input::Keycode>,
    pub input_mode: InputMode,
    shortcut_passthrough: bool,
    managed_fullscreen: bool,
    active_surface_session: SurfaceSessionId,
    last_interacted_surface_session: SurfaceSessionId,
    surface_session_interaction_order: Vec<SurfaceSessionId>,
    next_surface_session: u64,
    surface_sessions: Vec<SurfaceSessionId>,
    surface_session_names: Vec<(SurfaceSessionId, String)>,
    surface_session_working_directories: BTreeMap<SurfaceSessionId, PathBuf>,
    surface_session_directories: BTreeMap<PathBuf, Vec<SurfaceSessionId>>,
    surface_session_directory_defaults: BTreeMap<PathBuf, SurfaceSessionId>,
    window_surface_sessions: Vec<(WlSurface, SurfaceSessionId)>,
    presentation_child_surfaces: Vec<WlSurface>,
    presentation_member_surfaces: Vec<WlSurface>,
    panel_reservations: Vec<PanelReservation>,
    left_panel_resizing: bool,
    extension_registry: ExtensionRegistry,
    session_dbus_buses: Vec<(SessionBusRealm, SessionBusHandle)>,
    plugin_runtime: Option<PluginRuntime>,
    plugin_host_event_sender: channel::Sender<PluginHostEvent>,
    plugin_actor_messages: u64,
    plugin_actor_failures: u64,
    plugin_actor_last_status: Option<String>,
    pub(crate) presentation_redraw_deferred: bool,
    pub(crate) presentation_skip_post_submit_frame_callbacks: bool,
    pub(crate) test_surface_markers: bool,
    pub(crate) presenter: Option<Rc<PresenterHandle>>,
    pub(crate) wasm_key_handler: Option<WasmKeyHandler>,
    pub(crate) wasm_map_hook: Option<WasmMapHook>,
    pub(crate) test_harness: Option<TestHarnessRuntime>,
    pub(crate) test_harness_completion: Option<TestHarnessCompletion>,
    pub(crate) nested_inner_dbus_destination: Option<String>,
    pub(crate) nested_inner_dbus_address: Option<String>,
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
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);

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
        let plugin_host_event_sender = Self::init_plugin_host_event_source(event_loop);
        Self::init_window_lifecycle_timer(event_loop);
        let loop_handle = event_loop.handle();
        let loop_signal = event_loop.get_signal();

        let dmabuf = DmabufRuntime {
            #[cfg(feature = "shm-fallback")]
            shm_formats: MANDATORY_SHM_FORMATS,
            ..DmabufRuntime::default()
        };

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
            seat_state,
            data_device_state,
            dmabuf,
            popups: PopupManager::default(),
            seat,
            presentation: PresentationStatus::Detached,
            presentation_started: false,
            presentation_attach_failures: 0,
            presentation_runtime_failures: 0,
            presentation_last_error: None,
            presentation_window_focused: false,
            pending_presentation_focus_activation_token: None,
            attach_requested: false,
            attach_schedule_pending: false,
            detach_requested: false,
            browsers: BrowserManager::default(),
            layout: LayoutState::default(),
            accepted_resolved_layout: ResolvedLayout::empty(),
            resolved_layout_source: ResolvedLayoutSource::NativeFallback,
            stats: RuntimeStats::default(),
            next_window_slot: 0,
            last_window_count: 0,
            active_toplevel_surface: None,
            pointer_hover_toplevel_surface: None,
            pending_pointer_focus_surface: None,
            layout_leaf_selected_panes: Vec::new(),
            native_auto_split_visible_limit: DEFAULT_NATIVE_AUTO_SPLIT_VISIBLE_LIMIT,
            native_session_layouts: BTreeMap::new(),
            fullscreen_toplevels: Vec::new(),
            next_pane_id: 1,
            window_pane_ids: Vec::new(),
            pending_child_panes: Vec::new(),
            ctrl_pressed: false,
            alt_pressed: false,
            shift_pressed: false,
            ctrl_pressed_key: None,
            alt_pressed_key: None,
            shift_pressed_key: None,
            ctrl_forwarded_to_client: false,
            alt_forwarded_to_client: false,
            shift_forwarded_to_client: false,
            forwarded_pressed_keys: Vec::new(),
            suppress_mode_toggle_key_release: false,
            shortcut_prefix_active: false,
            suppress_shortcut_prefix_key_release: None,
            input_mode: InputMode::Passthrough,
            shortcut_passthrough: false,
            managed_fullscreen: false,
            active_surface_session: SurfaceSessionId(1),
            last_interacted_surface_session: SurfaceSessionId(1),
            surface_session_interaction_order: vec![SurfaceSessionId(1)],
            next_surface_session: 2,
            surface_sessions: vec![SurfaceSessionId(1)],
            surface_session_names: vec![(
                SurfaceSessionId(1),
                default_surface_session_name(SurfaceSessionId(1)),
            )],
            surface_session_working_directories: [(
                SurfaceSessionId(1),
                current_process_directory(),
            )]
            .into_iter()
            .collect(),
            surface_session_directories: BTreeMap::new(),
            surface_session_directory_defaults: BTreeMap::new(),
            window_surface_sessions: Vec::new(),
            presentation_child_surfaces: Vec::new(),
            presentation_member_surfaces: Vec::new(),
            panel_reservations: Vec::new(),
            left_panel_resizing: false,
            extension_registry: ExtensionRegistry::default(),
            session_dbus_buses: Vec::new(),
            plugin_runtime: None,
            plugin_host_event_sender,
            plugin_actor_messages: 0,
            plugin_actor_failures: 0,
            plugin_actor_last_status: None,
            presentation_redraw_deferred: false,
            presentation_skip_post_submit_frame_callbacks: false,
            test_surface_markers: false,
            presenter: None,
            wasm_key_handler: None,
            wasm_map_hook: None,
            test_harness: None,
            test_harness_completion: None,
            nested_inner_dbus_destination: None,
            nested_inner_dbus_address: None,
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
        let plugin_runtime = self
            .plugin_runtime
            .as_mut()
            .context("plugin runtime is not running")?;
        plugin_runtime.send_message(actor_id, sender, body)?;
        self.plugin_actor_messages += 1;
        self.plugin_actor_last_status = Some(format!("{actor_id}:message_queued"));
        Ok(())
    }

    pub fn plugin_actor_summaries(&self) -> Vec<PluginActorSummary> {
        self.plugin_runtime
            .as_ref()
            .map_or_else(Vec::new, PluginRuntime::actor_summaries)
    }

    fn handle_plugin_host_event(&mut self, event: PluginHostEvent) {
        match event {
            PluginHostEvent::SpawnFootRequested { actor_id, message } => {
                info!(
                    plugin_actor = %actor_id,
                    dbus_sender = %message.sender,
                    message = %message.body,
                    "plugin actor requested foot spawn"
                );
                let argv = vec!["foot".to_string()];
                match self.spawn_hosted_app_argv(&argv) {
                    Ok(()) => {
                        self.plugin_actor_last_status = Some(format!("{actor_id}:spawned_foot"));
                    }
                    Err(err) => {
                        self.plugin_actor_failures += 1;
                        self.plugin_actor_last_status = Some(format!("{actor_id}:spawn_error"));
                        warn!(
                            plugin_actor = %actor_id,
                            error = %err,
                            "plugin actor failed to spawn foot"
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

    pub const fn set_native_auto_split_visible_limit(&mut self, limit: usize) {
        self.native_auto_split_visible_limit = limit;
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
            None,
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
                    // Smithay's display source is stored in calloop's Generic wrapper
                    // and must not be dropped while this callback is active.
                    unsafe {
                        if let Err(err) = display.get_mut().dispatch_clients(state) {
                            state.stats.wayland_dispatch_errors += 1;
                            warn!(?err, "dispatch Wayland clients failed");
                        }
                    }
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
                        state.record_hosted_child_spawn_failure(failure);
                    }
                    HostedChildEvent::Exited(exit) => state.record_hosted_child_exit(exit),
                }
            })
            .expect("insert hosted child event source");
        sender
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

    pub fn spawn_hosted_app(&mut self, command: &OsString) -> Result<()> {
        self.spawn_hosted_app_in_session(command, self.active_surface_session)
    }

    pub fn spawn_hosted_app_in_session(
        &mut self,
        command: &OsString,
        session: SurfaceSessionId,
    ) -> Result<()> {
        ensure!(
            self.surface_sessions.contains(&session),
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
            false,
            format!(
                "spawn hosted Wayland app {:?} with WAYLAND_DISPLAY={}",
                command,
                self.socket_name.to_string_lossy()
            ),
        )
    }

    pub fn spawn_hosted_app_argv(&mut self, argv: &[String]) -> Result<()> {
        self.spawn_hosted_app_argv_in_session(argv, self.active_surface_session)
    }

    pub fn spawn_hosted_app_argv_in_session(
        &mut self,
        argv: &[String],
        session: SurfaceSessionId,
    ) -> Result<()> {
        let Some((program, args)) = argv.split_first() else {
            anyhow::bail!("spawn requires a command");
        };
        ensure!(
            self.surface_sessions.contains(&session),
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
            false,
            format!(
                "spawn hosted Wayland app {:?} with WAYLAND_DISPLAY={}",
                argv,
                self.socket_name.to_string_lossy()
            ),
        )
    }

    pub fn spawn_presentation_app_argv(&mut self, argv: &[String]) -> Result<()> {
        let Some((program, args)) = argv.split_first() else {
            anyhow::bail!("spawn requires a command");
        };

        let pane_id = self.allocate_pane_id();
        let session = self.active_surface_session;
        let mut command_builder = Command::new(program);
        command_builder
            .args(args)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env(MANIFOLD_PANE_ID_ENV, pane_id.to_string())
            .env(MANIFOLD_SURFACE_SESSION_ID_ENV, session.raw().to_string());
        self.apply_control_environment(&mut command_builder, session);
        command_builder.env(MANIFOLD_SESSION_ENV, "presentation");
        self.launch_hosted_child(
            argv.join(" "),
            command_builder,
            pane_id,
            session,
            true,
            format!(
                "spawn presentation app {:?} with WAYLAND_DISPLAY={}",
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
        presentation: bool,
        spawn_context: String,
    ) -> Result<()> {
        self.pending_child_panes.push(PendingChildPane {
            pid: None,
            pane_id,
            session,
            presentation,
        });
        if let Some(runtime) = self.async_runtime.clone() {
            self.spawn_hosted_child_launch_task(runtime, command_label, command, pane_id, session);
            return Ok(());
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
                Ok(())
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
        runtime: tokio::runtime::Handle,
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

    fn record_hosted_child_spawn_failure(&mut self, failure: HostedChildSpawnFailure) {
        self.pending_child_panes
            .retain(|pending| pending.pane_id != failure.pane_id);
        self.hosted_child_failures += 1;
        self.hosted_child_last_status = Some(format!("{}:spawn_error", failure.command));
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
                    record_hosted_child_exit_status(
                        &mut self.hosted_child_exits,
                        &mut self.hosted_child_failures,
                        &mut self.hosted_child_last_status,
                        &hosted.command,
                        hosted.pane_id,
                        status,
                    );
                }
                Ok(None) => active.push(hosted),
                Err(err) => {
                    record_hosted_child_exit_error(
                        &mut self.hosted_child_exits,
                        &mut self.hosted_child_failures,
                        &mut self.hosted_child_last_status,
                        &hosted.command,
                        hosted.pane_id,
                        &err,
                        "poll_error",
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
        match exit.result {
            Ok(status) => record_hosted_child_exit_status(
                &mut self.hosted_child_exits,
                &mut self.hosted_child_failures,
                &mut self.hosted_child_last_status,
                &exit.command,
                exit.pane_id,
                status,
            ),
            Err(err) => record_hosted_child_exit_error(
                &mut self.hosted_child_exits,
                &mut self.hosted_child_failures,
                &mut self.hosted_child_last_status,
                &exit.command,
                exit.pane_id,
                &err,
                "wait_error",
            ),
        }
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
        if let Some(directory) = self.surface_session_working_directories.get(&session) {
            child.current_dir(directory);
        }
    }

    fn ensure_session_dbus_bus(&mut self, session: SurfaceSessionId) -> Result<String> {
        let realm = SessionBusRealm::Surface(session.raw());
        if let Some(bus) = self
            .session_dbus_buses
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
        self.session_dbus_buses.push((realm, handle));
        info!(
            session = session.raw(),
            dbus_address = %address,
            "started in-process session-local DBus bus"
        );
        Ok(address)
    }

    pub fn request_attach(&mut self) {
        if self.presentation == PresentationStatus::Detached {
            self.stats.record_attach_request();
            self.attach_requested = true;
        }
    }

    pub fn request_detach(&mut self) {
        if self.presentation == PresentationStatus::Attached {
            self.stats.record_detach_request();
            self.detach_requested = true;
            self.pending_presentation_focus_activation_token = None;
        }
    }

    pub fn request_os_window_close(&mut self) {
        self.request_detach();
    }

    pub fn record_presentation_attach_failure(&mut self, error: &str) {
        self.stats.record_attach_failed();
        self.presentation_attach_failures += 1;
        self.presentation_last_error = Some(crate::presentation::sanitize_status_value(error));
        self.presentation = PresentationStatus::Detached;
        self.attach_requested = false;
        self.attach_schedule_pending = false;
        self.pending_presentation_focus_activation_token = None;
    }

    pub fn schedule_attach(&mut self) {
        let handle = self.loop_handle.clone();
        let result = self
            .loop_handle
            .insert_source(Timer::immediate(), move |_, (), state| {
                if let Err(err) = crate::presentation::attach(&handle, state) {
                    warn!(?err, "failed to attach presentation");
                    state.record_presentation_attach_failure(&err.to_string());
                }
                state.attach_schedule_pending = false;
                TimeoutAction::Drop
            });
        if let Err(err) = result {
            warn!(?err, "failed to schedule presentation attach");
            self.record_presentation_attach_failure(&format!("schedule_attach:{err}"));
        }
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.window_under_layout_position(pos)
            .and_then(|(window, location)| {
                let surface_origin =
                    window_surface_origin_in_compositor_space(location, window.geometry().loc);
                let local_pos = Point::from((
                    pos.x - f64::from(surface_origin.x),
                    pos.y - f64::from(surface_origin.y),
                ));
                window
                    .surface_under(local_pos, WindowSurfaceType::ALL)
                    .map(|(surface, point)| (surface, (surface_origin + point).to_f64()))
            })
    }

    pub fn window_under_layout_position(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(Window, Point<i32, Logical>)> {
        if let Some(panel_hit) = self
            .space
            .elements()
            .rev()
            .filter_map(|window| {
                Self::window_toplevel_surface(window)
                    .is_some_and(|surface| self.panel_rect_for_surface(&surface).is_some())
                    .then(|| {
                        self.window_hit_location(window, pos)
                            .map(|location| (window.clone(), location))
                    })
                    .flatten()
            })
            .next()
        {
            return Some(panel_hit);
        }

        if let Some(active_window) = self.active_window()
            && let Some(surface) = Self::window_toplevel_surface(&active_window)
            && self.toplevel_fullscreen_output(&surface).is_some()
            && let Some(location) = self.window_hit_location(&active_window, pos)
        {
            return Some((active_window, location));
        }

        let content_rect = self.content_rect();
        if !content_rect.contains_f64(pos.x, pos.y) {
            return None;
        }
        let point = LocalPoint::from_pixel_in_rect(pos.x, pos.y, content_rect);
        for hit in self.resolved_effective_hit_stack(point, pos) {
            match hit.layout_hit.kind {
                LayoutItemKind::Pane { .. } => {
                    if let Some((window, location)) = hit.window_hit {
                        return Some((window, location));
                    }
                }
                LayoutItemKind::Placeholder
                | LayoutItemKind::Handle
                | LayoutItemKind::Chrome
                | LayoutItemKind::Overlay => return None,
            }
        }
        None
    }

    pub const fn log_pointer_click_focus_debug(
        &self,
        _phase: &'static str,
        _button_code: u32,
        _pos: Point<f64, Logical>,
        _target_surface: Option<&WlSurface>,
        _focus_surface: Option<&WlSurface>,
    ) {
    }

    fn resolved_effective_hit_stack(
        &self,
        point: LocalPoint,
        pos: Point<f64, Logical>,
    ) -> Vec<ResolvedPresentationHit> {
        self.accepted_resolved_layout
            .hit_stack(point)
            .into_iter()
            .filter_map(|layout_hit| match layout_hit.kind {
                LayoutItemKind::Pane { pane } => {
                    let surface = self.surface_for_pane(pane)?;
                    let window = self.window_for_toplevel_surface(&surface)?;
                    let location = self.window_hit_location(&window, pos)?;
                    Some(ResolvedPresentationHit {
                        layout_hit,
                        window_hit: Some((window, location)),
                    })
                }
                LayoutItemKind::Placeholder
                | LayoutItemKind::Handle
                | LayoutItemKind::Chrome
                | LayoutItemKind::Overlay => Some(ResolvedPresentationHit {
                    layout_hit,
                    window_hit: None,
                }),
            })
            .collect()
    }

    fn window_hit_location(
        &self,
        window: &Window,
        pos: Point<f64, Logical>,
    ) -> Option<Point<i32, Logical>> {
        let rect = self.window_layout_rect(window)?;
        if !rect.contains_f64(pos.x, pos.y) {
            return None;
        }
        let surface_origin = window_surface_origin_in_compositor_space(
            Point::from((rect.x, rect.y)),
            window.geometry().loc,
        );
        let local_pos = Point::from((
            pos.x - f64::from(surface_origin.x),
            pos.y - f64::from(surface_origin.y),
        ));
        window
            .surface_under(local_pos, WindowSurfaceType::ALL)
            .is_some()
            .then(|| Point::from((rect.x, rect.y)))
    }

    pub fn first_window_center(&self) -> Option<Point<f64, Logical>> {
        self.space.elements().find_map(|window| {
            let rect = self.window_layout_rect(window)?;
            Some(Point::from((
                f64::from(rect.x) + f64::from(rect.w.max(1)) / 2.0,
                f64::from(rect.y) + f64::from(rect.h.max(1)) / 2.0,
            )))
        })
    }

    pub fn allocate_toplevel_location(&mut self) -> Point<i32, Logical> {
        let slot = self.next_window_slot;
        self.next_window_slot += 1;
        focus_stack_location(slot)
    }

    pub fn record_dmabuf_blocker_failure(&mut self, error: &str) {
        self.dmabuf.stats.blocker_install_failures += 1;
        self.dmabuf.stats.last_blocker_error =
            Some(crate::presentation::sanitize_status_value(error));
    }

    pub fn service_test_harness(&mut self) {
        let Some(mut harness) = self.test_harness.take() else {
            return;
        };

        for request in harness.take_pending_requests() {
            service_request(self, request);
        }

        if self.test_harness_completion.is_none()
            && let Some(completion) = harness.try_take_completion()
        {
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
            self.test_harness_completion = Some(completion);
        }

        self.test_harness = Some(harness);
    }

    pub fn signal_test_harness(&self, name: String) -> Result<()> {
        let Some(harness) = self.test_harness.as_ref() else {
            anyhow::bail!("no running test harness");
        };
        harness.send_signal(name)
    }

    pub fn activate_window_exclusively(&mut self, active_surface: Option<&WlSurface>) {
        self.space.elements().for_each(|window| {
            let Some(toplevel) = window.toplevel() else {
                return;
            };
            let layout_rect = self.window_layout_rect(window);
            let is_active = active_surface == Some(toplevel.wl_surface())
                && self.surface_can_receive_focus(toplevel.wl_surface());
            toplevel.with_pending_state(|state| {
                if let Some(rect) = layout_rect {
                    let is_fullscreen = self
                        .toplevel_fullscreen_output(toplevel.wl_surface())
                        .is_some();
                    let fullscreen_output = self
                        .toplevel_fullscreen_output(toplevel.wl_surface())
                        .cloned()
                        .flatten();
                    let client_size = (rect.w.max(1), rect.h.max(1));
                    state.size = Some(client_size.into());
                    state.bounds = Some(client_size.into());
                    if is_fullscreen || self.managed_fullscreen {
                        state.states.set(xdg_toplevel::State::Fullscreen);
                        state.states.unset(xdg_toplevel::State::Maximized);
                    } else {
                        state.states.set(xdg_toplevel::State::Maximized);
                        state.states.unset(xdg_toplevel::State::Fullscreen);
                    }
                    state.fullscreen_output = fullscreen_output;
                }
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
        self.last_window_count = self.active_surface_session_window_count();
        self.reconfigure_focus_stack();
    }

    pub fn assign_new_toplevel_surface(&mut self, surface: &WlSurface) -> Option<WlSurface> {
        let registered = self.register_toplevel_pane_and_session(surface);
        self.window_surface_sessions
            .push((surface.clone(), registered.session));
        self.apply_registered_extension_grants_for_surface(surface);
        if registered.presentation {
            self.mark_presentation_child_surface(surface);
            self.refresh_resolved_layout_from_native_policy();
            return self.active_content_surface_for_layout_action();
        }
        if self.resolved_layout_source == ResolvedLayoutSource::NativeFallback
            && registered.session == self.active_surface_session
        {
            self.place_new_native_pane(registered.pane_id);
        }
        self.apply_map_hook_for_pending_pane(registered.pane_id)
    }

    fn place_new_native_pane(&mut self, pane: u64) {
        match self.layout.active_kind() {
            Some(LayoutLeafKind::Client) => {
                let active_leaf = self.layout.active_leaf();
                let occupied_pane =
                    self.selected_pane_for_layout_leaf(active_leaf)
                        .filter(|selected| {
                            self.active_session_content_surface_for_pane(*selected)
                                .is_some()
                        });
                let Some(occupied_pane) = occupied_pane else {
                    self.set_layout_leaf_selected_pane(active_leaf, pane);
                    return;
                };
                if self.resolved_layout_pane_count() >= self.native_auto_split_visible_limit {
                    return;
                }
                let axis = self.smart_split_axis_for_active_leaf();
                if let Some(new_leaf) = self.layout.split_active(axis) {
                    self.set_layout_leaf_selected_pane(active_leaf, occupied_pane);
                    self.set_layout_leaf_selected_pane(new_leaf, pane);
                    self.layout.set_active_leaf(new_leaf);
                } else {
                    self.set_layout_leaf_selected_pane(active_leaf, pane);
                }
            }
            None => {}
        }
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
                presentation: self.toplevel_process_is_presentation(surface)
                    || self
                        .presentation_child_surfaces
                        .iter()
                        .any(|child| child == surface),
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
        let presentation = pending.as_ref().is_some_and(|pending| pending.presentation)
            || self.toplevel_process_is_presentation(surface);
        self.window_pane_ids.push((surface.clone(), pane));
        RegisteredToplevelPane {
            pane_id: pane,
            session,
            presentation,
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
            match kind.as_str() {
                "presentation-membership" => {
                    if !self
                        .presentation_member_surfaces
                        .iter()
                        .any(|member| member == surface)
                    {
                        self.presentation_member_surfaces.push(surface.clone());
                    }
                }
                "placement" => {
                    if let Some(session) = detail
                        .get("surface_session")
                        .or_else(|| detail.get("session_id"))
                        .and_then(|value| self.resolve_surface_session(value))
                    {
                        self.assign_toplevel_surface_session(surface, session);
                    }
                    if placement_requests_left_panel(&detail) {
                        let width =
                            placement_panel_width(&detail).unwrap_or(LEFT_PANEL_DEFAULT_WIDTH);
                        let visible = detail
                            .get("visible")
                            .and_then(|value| parse_boolish(value))
                            .unwrap_or(true);
                        self.reserve_left_panel(surface.clone(), width, visible);
                    }
                    if detail
                        .get("focus")
                        .is_some_and(|value| matches!(value.as_str(), "true" | "1" | "yes"))
                    {
                        self.focus_toplevel_surface(Some(surface.clone()));
                    }
                }
                _ => {}
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
            .filter(|session| self.surface_sessions.contains(session))
    }

    fn toplevel_process_is_presentation(&self, surface: &WlSurface) -> bool {
        let Some(pid) = toplevel_pid(self, surface).and_then(|pid| pid.parse::<u32>().ok()) else {
            return false;
        };
        process_tree_env_var(pid, MANIFOLD_SESSION_ENV).as_deref() == Some("presentation")
    }

    fn mark_presentation_child_surface(&mut self, surface: &WlSurface) {
        if !self
            .presentation_child_surfaces
            .iter()
            .any(|child| child == surface)
        {
            self.presentation_child_surfaces.push(surface.clone());
        }
    }

    fn default_surface_session(&self) -> SurfaceSessionId {
        self.surface_sessions
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

    fn active_surface_session_panes(&self) -> Vec<u64> {
        self.window_surface_sessions
            .iter()
            .filter(|(_, session)| *session == self.active_surface_session)
            .filter(|(surface, _)| self.surface_participates_in_resolved_layout(surface))
            .filter_map(|(surface, _)| self.pane_for_toplevel_surface(surface))
            .fold(Vec::new(), |mut panes, pane| {
                if !panes.contains(&pane) {
                    panes.push(pane);
                }
                panes
            })
    }

    fn leaf_for_toplevel_surface(&self, surface: &WlSurface) -> Option<LayoutLeafId> {
        let pane = self.pane_for_toplevel_surface(surface)?;
        self.layout_leaf_selected_panes
            .iter()
            .rev()
            .find_map(|(leaf, selected)| (*selected == pane).then_some(*leaf))
    }

    fn selected_pane_for_layout_leaf(&self, target_leaf: LayoutLeafId) -> Option<u64> {
        self.layout_leaf_selected_panes
            .iter()
            .rev()
            .find_map(|(leaf, pane)| (*leaf == target_leaf).then_some(*pane))
    }

    fn set_layout_leaf_selected_pane(&mut self, leaf: LayoutLeafId, pane: u64) {
        self.layout_leaf_selected_panes
            .retain(|(selected_leaf, _)| *selected_leaf != leaf);
        self.layout_leaf_selected_panes.push((leaf, pane));
    }

    fn active_session_content_surface_for_pane(&self, pane: u64) -> Option<WlSurface> {
        let surface = self.surface_for_pane(pane)?;
        (self
            .session_for_toplevel_surface(&surface)
            .unwrap_or(SurfaceSessionId(1))
            == self.active_surface_session
            && self.surface_participates_in_resolved_layout(&surface)
            && self.window_for_any_toplevel_surface(&surface).is_some())
        .then_some(surface)
    }

    fn apply_map_hook_for_pending_pane(&mut self, pending_pane: u64) -> Option<WlSurface> {
        let visible_panes = self.active_surface_session_panes();
        let Some(handler) = self.wasm_map_hook.as_mut() else {
            self.refresh_resolved_layout_from_native_policy();
            return self.surface_for_pane(pending_pane);
        };
        let input = MapHookInput {
            pending_pane,
            visible_panes,
        };
        let outcome = match handler.handle_map(input) {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!(
                    ?err,
                    pending_pane, "WASM map hook failed; using native placement"
                );
                self.refresh_resolved_layout_from_native_policy();
                return self.surface_for_pane(pending_pane);
            }
        };
        match outcome {
            MapHookOutcome::Abstain => {
                self.refresh_resolved_layout_from_native_policy();
                self.surface_for_pane(pending_pane)
            }
            MapHookOutcome::Response(response) => self
                .apply_map_hook_response(pending_pane, response)
                .or_else(|| self.surface_for_pane(pending_pane)),
        }
    }

    fn apply_map_hook_response(
        &mut self,
        pending_pane: u64,
        response: MapHookResponse,
    ) -> Option<WlSurface> {
        match response {
            MapHookResponse::Abstain => None,
            MapHookResponse::Reject { reason } => {
                warn!(
                    pending_pane,
                    reason, "WASM map hook rejected map; using native placement"
                );
                self.refresh_resolved_layout_from_native_policy();
                None
            }
            MapHookResponse::ResolvedLayout { layout, focus } => {
                self.apply_map_hook_resolved_layout(layout, focus)
            }
        }
    }

    fn apply_map_hook_resolved_layout(
        &mut self,
        layout: ResolvedLayout,
        focus: Option<u64>,
    ) -> Option<WlSurface> {
        let valid_panes = self.active_surface_session_panes();
        let layout = match layout.validate_for_panes(&valid_panes) {
            Ok(layout) => layout,
            Err(err) => {
                warn!(
                    ?err,
                    "WASM map hook returned invalid resolved layout; using native placement"
                );
                self.refresh_resolved_layout_from_native_policy();
                return None;
            }
        };

        let focus_surface = focus
            .filter(|pane| layout.pane_item(*pane).is_some())
            .and_then(|pane| self.surface_for_pane(pane));
        self.accepted_resolved_layout = layout;
        self.resolved_layout_source = ResolvedLayoutSource::WasmPolicy;
        self.reconfigure_focus_stack();
        focus_surface
    }

    fn refresh_resolved_layout_from_native_policy(&mut self) {
        self.repair_native_layout_leaf_selections();
        let layout = self.native_fallback_resolved_layout();
        match layout.validate_for_panes(&self.active_surface_session_panes()) {
            Ok(layout) => {
                self.accepted_resolved_layout = layout;
                self.resolved_layout_source = ResolvedLayoutSource::NativeFallback;
            }
            Err(err) => {
                warn!(?err, "native fallback produced invalid resolved layout");
                self.accepted_resolved_layout = ResolvedLayout::empty();
                self.resolved_layout_source = ResolvedLayoutSource::NativeFallback;
            }
        }
    }

    fn repair_native_layout_leaf_selections(&mut self) {
        let leaves = self
            .layout
            .resolve(self.presentation_layout_size())
            .into_iter()
            .filter(|region| region.kind == LayoutLeafKind::Client)
            .map(|region| region.leaf)
            .collect::<Vec<_>>();
        let panes = self.active_surface_session_panes();
        let old_selections = self.layout_leaf_selected_panes.clone();
        let mut used = Vec::new();
        let mut next_selections = Vec::new();

        for leaf in leaves {
            let preferred = old_selections
                .iter()
                .rev()
                .find_map(|(selected_leaf, pane)| (*selected_leaf == leaf).then_some(*pane));
            let selected = preferred
                .and_then(|pane| first_available_pane_from(&panes, pane, &used, 0))
                .or_else(|| panes.iter().copied().find(|pane| !used.contains(pane)));
            if let Some(pane) = selected {
                used.push(pane);
                next_selections.push((leaf, pane));
            }
        }

        self.layout_leaf_selected_panes = next_selections;
    }

    fn native_fallback_resolved_layout(&self) -> ResolvedLayout {
        let bounds = self.presentation_layout_size();
        let mut items = Vec::new();
        let mut z = 0.0;
        for region in self.layout.resolve(bounds) {
            let pane = self
                .selected_pane_for_layout_leaf(region.leaf)
                .filter(|pane| {
                    region.kind == LayoutLeafKind::Client
                        && self
                            .active_session_content_surface_for_pane(*pane)
                            .is_some()
                });
            let Some(pane) = pane else {
                items.push(ResolvedLayoutItem {
                    id: LayoutItemId(1_000_000 + region.leaf.raw()),
                    kind: LayoutItemKind::Placeholder,
                    z,
                    transform: LocalTransform::from_pixel_rect(region.rect, bounds),
                    input: InputBehavior::Hit,
                });
                z += 1.0;
                continue;
            };

            items.push(ResolvedLayoutItem {
                id: LayoutItemId(pane),
                kind: LayoutItemKind::Pane { pane },
                z,
                transform: LocalTransform::from_pixel_rect(region.rect, bounds),
                input: InputBehavior::Pane,
            });
            z += 1.0;
        }
        ResolvedLayout { items }
    }

    pub const fn active_surface_session(&self) -> SurfaceSessionId {
        self.active_surface_session
    }

    fn save_active_native_session_layout(&mut self) {
        self.native_session_layouts.insert(
            self.active_surface_session,
            NativeSessionLayout {
                layout: self.layout.clone(),
                leaf_selected_panes: self.layout_leaf_selected_panes.clone(),
            },
        );
    }

    fn restore_native_session_layout(&mut self, session: SurfaceSessionId) {
        let left_dock_visible = self.layout.left_dock_visible;
        let right_dock_visible = self.layout.right_dock_visible;
        let bottom_dock_visible = self.layout.bottom_dock_visible;
        let center_visible = self.layout.center_visible;
        if let Some(saved) = self.native_session_layouts.get(&session).cloned() {
            self.layout = saved.layout;
            self.layout_leaf_selected_panes = saved.leaf_selected_panes;
        } else {
            self.layout = LayoutState::default();
            self.layout_leaf_selected_panes.clear();
        }
        self.layout.left_dock_visible = left_dock_visible;
        self.layout.right_dock_visible = right_dock_visible;
        self.layout.bottom_dock_visible = bottom_dock_visible;
        self.layout.center_visible = center_visible;
    }

    pub fn restore_last_interacted_surface_session(&mut self) -> bool {
        let Some(session) = self.most_recent_live_surface_session() else {
            return false;
        };
        if self.active_surface_session == session {
            return false;
        }
        self.save_active_native_session_layout();
        self.active_surface_session = session;
        self.restore_native_session_layout(session);
        self.refresh_resolved_layout_from_native_policy();
        self.reconfigure_focus_stack();
        true
    }

    fn most_recent_live_surface_session(&self) -> Option<SurfaceSessionId> {
        self.surface_session_interaction_order
            .iter()
            .copied()
            .find(|session| {
                self.surface_sessions.contains(session)
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
        if !self.surface_sessions.contains(&session) {
            return;
        }
        self.last_interacted_surface_session = session;
        self.surface_session_interaction_order
            .retain(|existing| *existing != session);
        self.surface_session_interaction_order.insert(0, session);
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

        self.ctrl_pressed = false;
        self.alt_pressed = false;
        self.shift_pressed = false;
        self.ctrl_pressed_key = None;
        self.alt_pressed_key = None;
        self.shift_pressed_key = None;
        self.ctrl_forwarded_to_client = false;
        self.alt_forwarded_to_client = false;
        self.shift_forwarded_to_client = false;
        self.suppress_mode_toggle_key_release = false;
        self.shortcut_prefix_active = false;
        self.suppress_shortcut_prefix_key_release = None;
        self.pending_pointer_focus_surface = None;
        self.pointer_hover_toplevel_surface = None;
    }

    pub const fn surface_session_count(&self) -> usize {
        self.surface_sessions.len()
    }

    pub fn clear_initial_surface_session(&mut self) {
        if self.surface_sessions != [SurfaceSessionId(1)]
            || !self.window_surface_sessions.is_empty()
            || self.next_surface_session != 2
        {
            warn!("refusing to clear non-initial surface session state");
            return;
        }

        self.active_surface_session = SurfaceSessionId(1);
        self.last_interacted_surface_session = SurfaceSessionId(1);
        self.surface_session_interaction_order.clear();
        self.next_surface_session = 1;
        self.surface_sessions.clear();
        self.surface_session_names.clear();
        self.surface_session_working_directories.clear();
        self.surface_session_directories.clear();
        self.surface_session_directory_defaults.clear();
        self.native_session_layouts.clear();
    }

    pub fn surface_session_summaries(&self) -> Vec<SurfaceSessionSummary> {
        self.surface_sessions
            .iter()
            .copied()
            .filter_map(|session| self.surface_session_summary(session))
            .collect()
    }

    pub fn surface_session_summary(
        &self,
        session: SurfaceSessionId,
    ) -> Option<SurfaceSessionSummary> {
        self.surface_sessions
            .contains(&session)
            .then(|| SurfaceSessionSummary {
                id: session.raw(),
                name: self.surface_session_name(session).to_string(),
                active: session == self.active_surface_session,
                windows: self.surface_session_window_count(session),
                working_directory: self
                    .surface_session_working_directories
                    .get(&session)
                    .map(|directory| directory.to_string_lossy().into_owned()),
            })
    }

    pub fn rename_surface_session(&mut self, session: SurfaceSessionId, name: String) -> bool {
        let name = name.trim();
        if name.is_empty() || !self.surface_sessions.contains(&session) {
            return false;
        }
        self.surface_session_names
            .retain(|(assigned_session, _)| *assigned_session != session);
        self.surface_session_names.push((session, name.to_string()));
        true
    }

    pub fn active_surface_session_name(&self) -> &str {
        self.surface_session_name(self.active_surface_session)
    }

    pub fn resolve_directory_surface_session(
        &mut self,
        directory: &Path,
    ) -> Option<SurfaceSessionId> {
        self.prune_directory_surface_sessions(directory);
        let session = self
            .surface_session_directory_defaults
            .get(directory)
            .copied()?;
        if self.surface_sessions.contains(&session) {
            Some(session)
        } else {
            self.surface_session_directory_defaults.remove(directory);
            None
        }
    }

    pub fn directory_surface_session_summaries(
        &mut self,
        directory: &Path,
    ) -> Vec<SurfaceSessionSummary> {
        self.prune_directory_surface_sessions(directory);
        self.surface_session_directories
            .get(directory)
            .into_iter()
            .flatten()
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
        if !self.surface_sessions.contains(&session) {
            return false;
        }
        let sessions = self
            .surface_session_directories
            .entry(directory.clone())
            .or_default();
        if !sessions.contains(&session) {
            sessions.push(session);
        }
        self.surface_session_working_directories
            .entry(session)
            .or_insert_with(|| directory.clone());
        if set_default
            || !self
                .surface_session_directory_defaults
                .get(&directory)
                .is_some_and(|default| self.surface_sessions.contains(default))
        {
            self.surface_session_directory_defaults
                .insert(directory, session);
        }
        true
    }

    fn prune_directory_surface_sessions(&mut self, directory: &Path) {
        if let Some(sessions) = self.surface_session_directories.get_mut(directory) {
            sessions.retain(|session| self.surface_sessions.contains(session));
            if sessions.is_empty() {
                self.surface_session_directories.remove(directory);
            }
        }
        if self
            .surface_session_directory_defaults
            .get(directory)
            .is_some_and(|session| !self.surface_sessions.contains(session))
        {
            self.surface_session_directory_defaults.remove(directory);
        }
    }

    pub fn close_surface_session(&mut self, session: SurfaceSessionId) -> bool {
        if self.surface_sessions.len() <= 1 || !self.surface_sessions.contains(&session) {
            return false;
        }
        if self.active_surface_session != session {
            self.save_active_native_session_layout();
        }

        let surfaces = self
            .window_surface_sessions
            .iter()
            .filter(|&(_surface, assigned_session)| *assigned_session == session)
            .map(|(surface, _assigned_session)| surface.clone())
            .collect::<Vec<_>>();

        for surface in &surfaces {
            if let Some(window) = self.window_for_any_toplevel_surface(surface)
                && let Some(toplevel) = window.toplevel()
            {
                toplevel.send_close();
            }
        }

        self.surface_sessions
            .retain(|existing| *existing != session);
        self.surface_session_names
            .retain(|(assigned_session, _)| *assigned_session != session);
        self.surface_session_working_directories.remove(&session);
        self.surface_session_interaction_order
            .retain(|existing| *existing != session);
        self.surface_session_directories
            .retain(|_, assigned_sessions| {
                assigned_sessions.retain(|assigned_session| *assigned_session != session);
                !assigned_sessions.is_empty()
            });
        self.surface_session_directory_defaults
            .retain(|_, assigned_session| *assigned_session != session);
        self.window_surface_sessions
            .retain(|(_, assigned_session)| *assigned_session != session);
        self.native_session_layouts.remove(&session);
        if self.active_surface_session == session {
            self.active_surface_session = self
                .surface_session_interaction_order
                .iter()
                .copied()
                .find(|candidate| self.surface_sessions.contains(candidate))
                .or_else(|| self.surface_sessions.first().copied())
                .unwrap_or(SurfaceSessionId(1));
        }
        self.restore_native_session_layout(self.active_surface_session);
        self.refresh_resolved_layout_from_native_policy();
        self.reconfigure_focus_stack();
        true
    }

    pub fn resolve_surface_session(&self, query: &str) -> Option<SurfaceSessionId> {
        let query = query.trim();
        if query.is_empty() {
            return None;
        }
        if let Ok(raw) = query.parse::<u64>() {
            let session = SurfaceSessionId(raw);
            if self.surface_sessions.contains(&session) {
                return Some(session);
            }
        }
        self.surface_sessions.iter().copied().find(|session| {
            self.surface_session_name(*session)
                .eq_ignore_ascii_case(query)
        })
    }

    pub fn surface_session_name(&self, session: SurfaceSessionId) -> &str {
        self.surface_session_names
            .iter()
            .rev()
            .find_map(|(assigned_session, name)| (*assigned_session == session).then_some(name))
            .map_or("untitled", String::as_str)
    }

    pub fn active_surface_session_window_count(&self) -> usize {
        self.surface_session_window_count(self.active_surface_session)
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
        if !self.surface_sessions.contains(&session) || self.active_surface_session == session {
            return false;
        }
        self.save_active_native_session_layout();
        self.active_surface_session = session;
        self.restore_native_session_layout(session);
        self.record_surface_session_interaction(session);
        self.refresh_resolved_layout_from_native_policy();
        self.reconfigure_focus_stack();
        true
    }

    pub fn switch_to_next_surface_session(&mut self) -> bool {
        let Some(active_index) = self
            .surface_sessions
            .iter()
            .position(|session| *session == self.active_surface_session)
        else {
            self.active_surface_session = self
                .surface_sessions
                .first()
                .copied()
                .unwrap_or(SurfaceSessionId(1));
            self.restore_native_session_layout(self.active_surface_session);
            self.reconfigure_focus_stack();
            return true;
        };
        if self.surface_sessions.len() <= 1 {
            return false;
        }
        self.save_active_native_session_layout();
        self.active_surface_session =
            self.surface_sessions[(active_index + 1) % self.surface_sessions.len()];
        self.restore_native_session_layout(self.active_surface_session);
        self.record_surface_session_interaction(self.active_surface_session);
        self.refresh_resolved_layout_from_native_policy();
        self.reconfigure_focus_stack();
        true
    }

    pub fn create_and_attach_surface_session(&mut self) -> SurfaceSessionId {
        self.create_and_attach_named_surface_session(None)
    }

    pub fn create_and_attach_named_surface_session(
        &mut self,
        name: Option<String>,
    ) -> SurfaceSessionId {
        self.create_and_attach_named_surface_session_in_directory(name, current_process_directory())
    }

    pub fn create_and_attach_named_surface_session_in_directory(
        &mut self,
        name: Option<String>,
        directory: PathBuf,
    ) -> SurfaceSessionId {
        let session = SurfaceSessionId(self.next_surface_session);
        self.next_surface_session += 1;
        self.surface_sessions.push(session);
        self.surface_session_names.push((
            session,
            name.filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| default_surface_session_name(session)),
        ));
        self.surface_session_working_directories
            .insert(session, directory);
        self.save_active_native_session_layout();
        self.active_surface_session = session;
        self.restore_native_session_layout(session);
        self.record_surface_session_interaction(session);
        self.refresh_resolved_layout_from_native_policy();
        self.reconfigure_focus_stack();
        session
    }

    #[cfg(test)]
    pub fn surface_session_working_directory(&self, session: SurfaceSessionId) -> Option<&Path> {
        self.surface_session_working_directories
            .get(&session)
            .map(PathBuf::as_path)
    }

    fn session_for_toplevel_surface(&self, surface: &WlSurface) -> Option<SurfaceSessionId> {
        self.window_surface_sessions
            .iter()
            .rev()
            .find_map(|(assigned_surface, session)| {
                (assigned_surface == surface).then_some(*session)
            })
    }

    pub fn window_belongs_to_active_surface_session(&self, window: &Window) -> bool {
        let Some(surface) = Self::window_toplevel_surface(window) else {
            return false;
        };
        self.session_for_toplevel_surface(&surface)
            .unwrap_or(SurfaceSessionId(1))
            == self.active_surface_session
    }

    pub fn window_visible_in_presentation(&self, window: &Window) -> bool {
        let Some(surface) = Self::window_toplevel_surface(window) else {
            return false;
        };
        if self.surface_is_visible_presentation_member(&surface)
            || self.surface_has_visible_panel(&surface)
        {
            return true;
        }
        if !self.window_belongs_to_active_surface_session(window) {
            return false;
        }
        if self.toplevel_fullscreen_output(&surface).is_some() {
            return true;
        }
        self.pane_for_toplevel_surface(&surface)
            .is_some_and(|pane| self.accepted_resolved_layout.pane_item(pane).is_some())
    }

    fn surface_participates_in_resolved_layout(&self, surface: &WlSurface) -> bool {
        !self
            .presentation_child_surfaces
            .iter()
            .any(|child| child == surface)
            && !self
                .presentation_member_surfaces
                .iter()
                .any(|member| member == surface)
            && !self
                .panel_reservations
                .iter()
                .any(|panel| panel.surface == *surface)
    }

    fn surface_is_visible_presentation_member(&self, surface: &WlSurface) -> bool {
        if !self
            .presentation_member_surfaces
            .iter()
            .any(|member| member == surface)
        {
            return false;
        }
        self.panel_reservations
            .iter()
            .rev()
            .find(|panel| panel.surface == *surface)
            .is_none_or(|panel| panel.visible)
    }

    fn surface_has_visible_panel(&self, surface: &WlSurface) -> bool {
        self.panel_reservations
            .iter()
            .rev()
            .find(|panel| panel.surface == *surface)
            .is_some_and(|panel| panel.visible)
    }

    fn surface_can_receive_focus(&self, surface: &WlSurface) -> bool {
        self.surface_has_visible_pane_item(surface)
            || self.surface_is_visible_presentation_member(surface)
            || self.surface_has_visible_panel(surface)
    }

    pub fn presentation_visible_window_count(&self) -> usize {
        self.space
            .elements()
            .filter(|window| self.window_visible_in_presentation(window))
            .count()
    }

    pub fn window_layout_rect(&self, window: &Window) -> Option<LayoutRect> {
        if !self.window_visible_in_presentation(window) {
            return None;
        }
        let surface = Self::window_toplevel_surface(window)?;
        self.layout_rect_for_toplevel_surface(&surface)
    }

    pub(crate) fn window_test_marker_id(&self, window: &Window, fallback_index: usize) -> u64 {
        let Some(surface) = Self::window_toplevel_surface(window) else {
            return fallback_marker_id(fallback_index);
        };
        self.pane_for_toplevel_surface(&surface)
            .unwrap_or_else(|| fallback_marker_id(fallback_index))
    }

    fn layout_rect_for_toplevel_surface(&self, surface: &WlSurface) -> Option<LayoutRect> {
        if let Some(rect) = self.panel_rect_for_surface(surface) {
            return Some(rect);
        }
        if self.toplevel_fullscreen_output(surface).is_some() {
            return Some(
                self.translate_content_rect(fullscreen_layout_rect(
                    self.presentation_layout_size(),
                )),
            );
        }
        if !self.surface_participates_in_resolved_layout(surface) {
            return None;
        }
        let pane = self.pane_for_toplevel_surface(surface)?;
        let item = self.accepted_resolved_layout.pane_item(pane)?;
        Some(self.translate_content_rect(item.pixel_rect(self.presentation_layout_size())))
    }

    fn surface_has_visible_pane_item(&self, surface: &WlSurface) -> bool {
        if !self.surface_participates_in_resolved_layout(surface) {
            return false;
        }
        if self.toplevel_fullscreen_output(surface).is_some() {
            return true;
        }
        self.pane_for_toplevel_surface(surface)
            .is_some_and(|pane| self.accepted_resolved_layout.pane_item(pane).is_some())
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
        self.managed_fullscreen
            || self
                .active_toplevel_surface
                .as_ref()
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

    fn surface_for_layout_leaf(&self, target_leaf: LayoutLeafId) -> Option<WlSurface> {
        self.selected_pane_for_layout_leaf(target_leaf)
            .and_then(|pane| self.active_session_content_surface_for_pane(pane))
    }

    pub fn focus_toplevel_surface(&mut self, active_surface: Option<WlSurface>) {
        if let Some(session) = active_surface
            .as_ref()
            .and_then(|surface| self.session_for_toplevel_surface(surface))
        {
            self.record_surface_session_interaction(session);
        }
        if let Some(leaf) = active_surface
            .as_ref()
            .filter(|surface| self.surface_participates_in_resolved_layout(surface))
            .and_then(|surface| self.leaf_for_toplevel_surface(surface))
        {
            self.layout.set_active_leaf(leaf);
        }
        self.active_toplevel_surface.clone_from(&active_surface);
        if self.resolved_layout_source == ResolvedLayoutSource::NativeFallback {
            self.refresh_resolved_layout_from_native_policy();
        }
        self.activate_window_exclusively(active_surface.as_ref());
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard_focus =
            active_surface.filter(|surface| self.surface_can_receive_focus(surface));
        self.seat
            .get_keyboard()
            .expect("keyboard")
            .set_focus(self, keyboard_focus, serial);
    }

    fn fallback_focus_leaf_for_surface(&self, surface: &WlSurface) -> Option<LayoutLeafId> {
        self.leaf_for_toplevel_surface(surface).or_else(|| {
            self.layout
                .active_kind()
                .is_some()
                .then_some(self.layout.active_leaf())
        })
    }

    pub fn focus_pane(&mut self, pane: u64) -> bool {
        let Some(surface) = self.active_session_content_surface_for_pane(pane) else {
            return false;
        };
        if self.resolved_layout_source == ResolvedLayoutSource::NativeFallback {
            let Some(leaf) = self.fallback_focus_leaf_for_surface(&surface) else {
                return false;
            };
            self.layout.set_active_leaf(leaf);
            self.set_layout_leaf_selected_pane(leaf, pane);
            self.refresh_resolved_layout_from_native_policy();
        } else if !self.surface_has_visible_pane_item(&surface) {
            return false;
        }
        if let Some(window) = self.window_for_any_toplevel_surface(&surface) {
            self.space.raise_element(&window, true);
        }
        self.focus_toplevel_surface(Some(surface));
        self.reconfigure_focus_stack();
        true
    }

    pub fn active_window(&self) -> Option<Window> {
        self.active_toplevel_surface.as_ref().and_then(|active| {
            self.space
                .elements()
                .find(|window| {
                    self.window_visible_in_presentation(window)
                        && Self::window_toplevel_surface(window).as_ref() == Some(active)
                })
                .cloned()
        })
    }

    pub fn active_presentation_window(&self) -> Option<Window> {
        self.active_toplevel_surface
            .as_ref()
            .and_then(|active| self.window_for_toplevel_surface(active))
    }

    pub fn focus_window(&mut self, window: &Window) {
        self.space.raise_element(window, true);
        let Some(surface) = Self::window_toplevel_surface(window) else {
            self.focus_toplevel_surface(None);
            return;
        };
        if self
            .session_for_toplevel_surface(&surface)
            .unwrap_or(SurfaceSessionId(1))
            == self.active_surface_session
            && self.surface_participates_in_resolved_layout(&surface)
            && let Some(pane) = self.pane_for_toplevel_surface(&surface)
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
        if self.presentation_window_focused && !focused {
            self.reset_transient_input_state();
        }
        self.presentation_window_focused = focused;
    }

    pub const fn presentation_window_focused(&self) -> bool {
        self.presentation_window_focused
    }

    pub fn agent_has_focused_window(&self, agent_id: &str) -> bool {
        if self.layout.active_kind() != Some(LayoutLeafKind::Client) {
            return false;
        }
        let Some(record) = self
            .extension_registry
            .agents
            .iter()
            .find(|record| record.id == agent_id)
        else {
            return false;
        };
        if record.session != self.active_surface_session {
            return false;
        }
        let Some(surface) = self.active_toplevel_surface.as_ref() else {
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
        activation_token: &str,
    ) -> bool {
        let token = (!activation_token.is_empty()).then(|| activation_token.to_string());
        if self.focus_presentation_window_now(token.as_deref()) {
            self.pending_presentation_focus_activation_token = None;
            return true;
        }
        if self.attach_requested {
            self.pending_presentation_focus_activation_token = Some(token.unwrap_or_default());
        }
        false
    }

    pub(crate) const fn take_pending_presentation_focus_activation_token(
        &mut self,
    ) -> Option<String> {
        self.pending_presentation_focus_activation_token.take()
    }

    fn focus_presentation_window_now(&self, activation_token: Option<&str>) -> bool {
        self.presenter.as_ref().is_some_and(|presenter| {
            presenter
                .runtime
                .borrow()
                .backend
                .as_ref()
                .is_some_and(|backend| backend.focus_window(activation_token))
        })
    }

    pub fn focus_app_id_or_spawn(&mut self, app_id: &str, argv: &[String]) -> bool {
        if self.focus_app_id(app_id) {
            return true;
        }
        match self.spawn_hosted_app_argv(argv) {
            Ok(()) => true,
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

    pub fn focused_pane(&self) -> Option<u64> {
        let surface = self.active_toplevel_surface.as_ref()?;
        self.surface_has_visible_pane_item(surface)
            .then(|| self.pane_for_toplevel_surface(surface))
            .flatten()
    }

    pub fn active_session_wasm_key_panes(&self) -> Vec<WasmKeyPane> {
        self.space
            .elements()
            .filter(|window| self.window_belongs_to_active_surface_session(window))
            .filter_map(|window| {
                let surface = Self::window_toplevel_surface(window)?;
                if !self.surface_participates_in_resolved_layout(&surface) {
                    return None;
                }
                Some(WasmKeyPane {
                    pane: self.pane_for_toplevel_surface(&surface)?,
                    app_id: toplevel_app_id(&surface),
                })
            })
            .collect()
    }

    pub fn register_extension(
        &mut self,
        role: String,
        metadata: ExtensionMetadata,
        sender: String,
    ) -> Result<String> {
        let role = normalize_registry_value(&role).context("extension role must not be empty")?;
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
        kind: String,
        metadata: ExtensionMetadata,
        sender: String,
    ) -> Result<String> {
        let kind = normalize_registry_value(&kind).context("agent kind must not be empty")?;
        ensure_metadata(&metadata)?;
        let session = metadata
            .get("surface_session")
            .or_else(|| metadata.get("session_id"))
            .and_then(|value| self.resolve_surface_session(value))
            .unwrap_or(self.active_surface_session);
        let id = stable_agent_registry_id(&kind, &metadata);
        let status = metadata
            .get("status")
            .and_then(|value| normalize_registry_value(value));
        if let Some(record) = self
            .extension_registry
            .agents
            .iter_mut()
            .find(|record| record.id == id)
        {
            record.kind = kind;
            if let Some(status) = status {
                record.status = status;
            }
            merge_agent_metadata(&mut record.metadata, metadata);
            record.sender = sender;
            record.session = session;
        } else {
            self.extension_registry.agents.push(AgentRecord {
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

    pub fn update_agent(
        &mut self,
        agent_id: &str,
        status: String,
        metadata: ExtensionMetadata,
        sender: String,
    ) -> Result<bool> {
        let status = normalize_registry_value(&status).context("agent status must not be empty")?;
        ensure_metadata(&metadata)?;
        let Some(record) = self
            .extension_registry
            .agents
            .iter_mut()
            .find(|record| record.id == agent_id)
        else {
            return Ok(false);
        };
        record.status = status;
        merge_agent_metadata(&mut record.metadata, metadata);
        record.sender = sender;
        Ok(true)
    }

    pub fn notify_agent(
        &mut self,
        agent_id: &str,
        title: String,
        subtitle: String,
        body: String,
        level: String,
        metadata: ExtensionMetadata,
        sender: String,
    ) -> Result<String> {
        let title =
            normalize_registry_value(&title).context("notification title must not be empty")?;
        let level = normalize_registry_value(&level).unwrap_or_else(|| "info".to_string());
        ensure_metadata(&metadata)?;
        let Some(record) = self
            .extension_registry
            .agents
            .iter_mut()
            .find(|record| record.id == agent_id)
        else {
            anyhow::bail!("unknown agent {agent_id}");
        };
        record.sender = sender;
        record.attention = true;
        record.unread_notifications = record.unread_notifications.saturating_add(1);
        if matches!(level.as_str(), "waiting" | "blocked" | "error") {
            record.status.clone_from(&level);
        }

        let mut id_metadata = metadata.clone();
        id_metadata.insert("agent_id".to_string(), agent_id.to_string());
        id_metadata.insert("title".to_string(), title.clone());
        id_metadata.insert("body".to_string(), body.clone());
        let id = stable_registry_id("agent-notification", [&level], &id_metadata);
        self.extension_registry
            .notifications
            .push(AgentNotificationRecord {
                id: id.clone(),
                agent_id: agent_id.to_string(),
                title,
                subtitle,
                body,
                level,
                metadata,
                unread: true,
            });
        Ok(id)
    }

    pub fn clear_agent_attention(&mut self, agent_id: &str) -> bool {
        let Some(record) = self
            .extension_registry
            .agents
            .iter_mut()
            .find(|record| record.id == agent_id)
        else {
            return false;
        };
        record.attention = false;
        record.unread_notifications = 0;
        for notification in self
            .extension_registry
            .notifications
            .iter_mut()
            .filter(|notification| notification.agent_id == agent_id)
        {
            notification.unread = false;
        }
        true
    }

    pub fn unregister_agent(&mut self, agent_id: &str) -> bool {
        let before = self.extension_registry.agents.len();
        self.extension_registry
            .agents
            .retain(|agent| agent.id != agent_id);
        if before == self.extension_registry.agents.len() {
            return false;
        }
        self.extension_registry
            .notifications
            .retain(|notification| notification.agent_id != agent_id);
        true
    }

    pub fn unregister_agent_by_identity(
        &mut self,
        kind: String,
        metadata: ExtensionMetadata,
    ) -> Result<Option<String>> {
        let kind = normalize_registry_value(&kind).context("agent kind must not be empty")?;
        ensure_metadata(&metadata)?;
        let id = stable_agent_registry_id(&kind, &metadata);
        Ok(self.unregister_agent(&id).then_some(id))
    }

    fn unregister_agents_for_pane(&mut self, pane_id: u64) -> Vec<String> {
        let pane_id = pane_id.to_string();
        let removed = self
            .extension_registry
            .agents
            .iter()
            .filter(|agent| agent.metadata.get("pane_id") == Some(&pane_id))
            .map(|agent| agent.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if removed.is_empty() {
            return Vec::new();
        }
        self.extension_registry
            .agents
            .retain(|agent| !removed.contains(&agent.id));
        self.extension_registry
            .notifications
            .retain(|notification| !removed.contains(&notification.agent_id));
        removed.into_iter().collect()
    }

    pub fn register_status_publisher(
        &mut self,
        role: String,
        metadata: ExtensionMetadata,
        sender: String,
    ) -> Result<String> {
        let role =
            normalize_registry_value(&role).context("status publisher role must not be empty")?;
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
        publisher: String,
        channel: String,
        state: String,
        label: String,
        metadata: ExtensionMetadata,
    ) -> Result<bool> {
        let publisher =
            normalize_registry_value(&publisher).context("status publisher must not be empty")?;
        if publisher != MANIFOLD_STATUS_PUBLISHER {
            self.ensure_status_publisher_exists(&publisher)?;
        }
        let channel =
            normalize_registry_value(&channel).context("status channel must not be empty")?;
        let state = normalize_registry_value(&state).context("status state must not be empty")?;
        let label = normalize_registry_value(&label).unwrap_or_else(|| channel.clone());
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

    pub const fn shortcut_passthrough(&self) -> bool {
        self.shortcut_passthrough
    }

    pub fn set_shortcut_passthrough(&mut self, enabled: bool) -> bool {
        if self.shortcut_passthrough == enabled {
            return false;
        }
        self.shortcut_passthrough = enabled;
        if enabled {
            let mut metadata = ExtensionMetadata::new();
            metadata.insert("scope".to_string(), "workspace".to_string());
            metadata.insert("click_action".to_string(), "disable".to_string());
            let _ = self.publish_status(
                MANIFOLD_STATUS_PUBLISHER.to_string(),
                PASSTHROUGH_STATUS_CHANNEL.to_string(),
                "active".to_string(),
                "Passthrough".to_string(),
                metadata,
            );
        } else {
            self.clear_status(MANIFOLD_STATUS_PUBLISHER, PASSTHROUGH_STATUS_CHANNEL);
        }
        true
    }

    pub fn toggle_shortcut_passthrough(&mut self) -> bool {
        let enabled = !self.shortcut_passthrough;
        self.set_shortcut_passthrough(enabled);
        enabled
    }

    pub(crate) fn replace_wasm_key_handler(&mut self, path: &Path) -> Result<()> {
        let handler = WasmKeyHandler::load(path)?;
        self.wasm_key_handler = Some(handler);
        Ok(())
    }

    pub(crate) fn replace_wasm_map_hook(&mut self, path: &Path) -> Result<()> {
        let hook = WasmMapHook::load(path)?;
        self.wasm_map_hook = Some(hook);
        Ok(())
    }

    pub fn register_extension_window(
        &mut self,
        extension_id: String,
        identity: ExtensionMetadata,
        role: String,
        sender: String,
    ) -> Result<String> {
        self.ensure_extension_exists(&extension_id)?;
        let role = normalize_registry_value(&role).context("window role must not be empty")?;
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

    pub fn request_window_placement(
        &mut self,
        window_id: &str,
        placement: ExtensionMetadata,
        sender: String,
    ) -> Result<String> {
        ensure_metadata(&placement)?;
        let window = self.window_record(window_id)?;
        let mut detail = placement.clone();
        detail.insert("window_id".to_string(), window_id.to_string());
        let grant_id = self.record_grant(&window.extension_id, "placement", detail, sender);

        let Some(surface) = self.find_surface_for_window_identity(&window.identity) else {
            warn!(
                window_id,
                identity = ?window.identity,
                "DBus placement request granted but no matching toplevel is mapped yet"
            );
            return Ok(grant_id);
        };

        if let Some(session) = placement
            .get("surface_session")
            .or_else(|| placement.get("session_id"))
            .and_then(|value| self.resolve_surface_session(value))
        {
            self.assign_toplevel_surface_session(&surface, session);
        }
        if placement_requests_left_panel(&placement) {
            let width = placement_panel_width(&placement).unwrap_or(LEFT_PANEL_DEFAULT_WIDTH);
            let visible = placement
                .get("visible")
                .and_then(|value| parse_boolish(value))
                .unwrap_or(true);
            self.reserve_left_panel(surface.clone(), width, visible);
        }
        if placement
            .get("focus")
            .is_some_and(|value| matches!(value.as_str(), "true" | "1" | "yes"))
        {
            if let Some(session) = self.session_for_toplevel_surface(&surface) {
                self.switch_to_surface_session(session);
            }
            self.focus_toplevel_surface(Some(surface));
        } else {
            self.reconfigure_focus_stack();
        }
        Ok(grant_id)
    }

    pub fn replace_window_role(
        &mut self,
        window_id: &str,
        role: String,
        options: ExtensionMetadata,
        _sender: String,
    ) -> Result<bool> {
        let window = self.window_record(window_id)?;
        let role = normalize_registry_value(&role).context("replacement role must not be empty")?;
        ensure_metadata(&options)?;
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

        self.presentation_member_surfaces
            .retain(|member| member != &surface);
        self.panel_reservations
            .retain(|panel| panel.surface != surface);

        if self.active_toplevel_surface.as_ref() == Some(&surface) {
            self.focus_toplevel_surface(None);
        }

        if dismiss
            && let Some(window) = self.window_for_any_toplevel_surface(&surface)
            && let Some(toplevel) = window.toplevel()
        {
            toplevel.send_close();
        }

        self.repair_resolved_layout_for_current_inventory();
        self.reconfigure_focus_stack();
    }

    fn reserve_left_panel(&mut self, surface: WlSurface, width: i32, visible: bool) {
        self.panel_reservations
            .retain(|panel| panel.edge != PanelEdge::Left || panel.surface != surface);
        let (presentation_w, _) = self.presentation_size();
        let max_width = LEFT_PANEL_MAX_WIDTH.min((presentation_w - 120).max(LEFT_PANEL_MIN_WIDTH));
        let width = width.clamp(LEFT_PANEL_MIN_WIDTH, max_width);
        self.panel_reservations.push(PanelReservation {
            surface,
            edge: PanelEdge::Left,
            width,
            visible,
        });
        self.layout.left_dock_visible = visible;
        self.repair_resolved_layout_for_current_inventory();
        self.reconfigure_focus_stack();
    }

    pub fn request_presentation_membership(
        &mut self,
        window_id: &str,
        membership: ExtensionMetadata,
        sender: String,
    ) -> Result<String> {
        ensure_metadata(&membership)?;
        let window = self.window_record(window_id)?;
        let mut detail = membership;
        detail.insert("window_id".to_string(), window_id.to_string());
        let grant_id = self.record_grant(
            &window.extension_id,
            "presentation-membership",
            detail,
            sender,
        );

        let Some(surface) = self.find_surface_for_window_identity(&window.identity) else {
            warn!(
                window_id,
                identity = ?window.identity,
                "DBus presentation membership granted but no matching toplevel is mapped yet"
            );
            return Ok(grant_id);
        };

        if !self
            .presentation_member_surfaces
            .iter()
            .any(|member| member == &surface)
        {
            self.presentation_member_surfaces.push(surface);
        }
        self.repair_resolved_layout_for_current_inventory();
        self.reconfigure_focus_stack();
        Ok(grant_id)
    }

    pub fn request_shortcut_grant(
        &mut self,
        extension_id: String,
        shortcut: String,
        scope: String,
        label: String,
        sender: String,
    ) -> Result<String> {
        self.ensure_extension_exists(&extension_id)?;
        let shortcut = normalize_registry_value(&shortcut).context("shortcut must not be empty")?;
        ensure!(
            !manifold_owned_shortcut(&shortcut),
            "shortcut conflicts with an existing Manifold-owned shortcut"
        );
        let mut detail = ExtensionMetadata::new();
        detail.insert("shortcut".to_string(), shortcut);
        detail.insert("scope".to_string(), scope);
        detail.insert("label".to_string(), label);
        Ok(self.record_grant(&extension_id, "shortcut", detail, sender))
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
        self.space
            .elements()
            .find(|window| {
                self.window_visible_in_presentation(window)
                    && Self::window_toplevel_surface(window).as_ref() == Some(surface)
            })
            .cloned()
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
        if !self.surface_sessions.contains(&session) {
            return false;
        }
        self.window_surface_sessions
            .retain(|(assigned_surface, _)| assigned_surface != surface);
        self.window_surface_sessions
            .push((surface.clone(), session));
        self.refresh_resolved_layout_from_native_policy();
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
        self.pending_pointer_focus_surface = surface;
    }

    pub const fn take_pending_pointer_focus_surface(&mut self) -> Option<WlSurface> {
        self.pending_pointer_focus_surface.take()
    }

    pub fn set_pointer_hover_toplevel_surface(&mut self, surface: Option<WlSurface>) {
        self.pointer_hover_toplevel_surface = surface;
    }

    pub fn cycle_focus(&mut self, direction: FocusDirection) -> bool {
        let windows = self
            .space
            .elements()
            .filter(|window| self.window_visible_in_presentation(window))
            .cloned()
            .collect::<Vec<_>>();
        let window_count = windows.len();
        if window_count == 0 {
            self.focus_toplevel_surface(None);
            return false;
        }

        let active_index = self
            .active_toplevel_surface
            .as_ref()
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
        self.focus_window(&windows[next_index]);
        true
    }

    fn cycle_focus_in_active_layout_leaf(&mut self, direction: FocusDirection) -> bool {
        let Some(active_surface) = self.active_toplevel_surface.clone() else {
            return false;
        };
        if !self.surface_has_visible_pane_item(&active_surface) {
            return false;
        }
        let Some(active_leaf) = self.leaf_for_toplevel_surface(&active_surface) else {
            return false;
        };
        let Some(current_pane) = self.pane_for_toplevel_surface(&active_surface) else {
            return false;
        };
        let excluded = self.selected_panes_for_other_layout_leaves(active_leaf);
        let step = match direction {
            FocusDirection::Next => 1,
            FocusDirection::Previous => -1,
        };
        let Some(next_pane) = first_available_pane_from(
            &self.active_surface_session_panes(),
            current_pane,
            &excluded,
            step,
        ) else {
            return false;
        };
        if next_pane == current_pane {
            return false;
        }
        self.set_layout_leaf_selected_pane(active_leaf, next_pane);
        self.refresh_resolved_layout_from_native_policy();
        let Some(next_surface) = self.surface_for_pane(next_pane) else {
            return false;
        };
        self.focus_toplevel_surface(Some(next_surface));
        self.reconfigure_focus_stack();
        true
    }

    fn split_active_content_leaf(&mut self, axis: LayoutAxis) -> bool {
        if self.resolved_layout_source != ResolvedLayoutSource::NativeFallback {
            return false;
        }
        let Some(active_surface) = self.active_content_surface_for_layout_action() else {
            return false;
        };
        let Some(active_leaf) = self.leaf_for_toplevel_surface(&active_surface) else {
            return false;
        };
        if !self.layout.set_active_leaf(active_leaf) {
            return false;
        }
        let Some(new_leaf) = self.layout.split_active(axis) else {
            return false;
        };

        if let Some(active_pane) = self.pane_for_toplevel_surface(&active_surface) {
            self.set_layout_leaf_selected_pane(active_leaf, active_pane);
            if let Some(next_pane) = first_available_pane_from(
                &self.active_surface_session_panes(),
                active_pane,
                &[active_pane],
                1,
            ) {
                self.set_layout_leaf_selected_pane(new_leaf, next_pane);
            }
        }

        self.refresh_resolved_layout_from_native_policy();
        self.reconfigure_focus_stack();
        true
    }

    fn split_active_content_leaf_smart(&mut self) -> bool {
        if self.resolved_layout_source != ResolvedLayoutSource::NativeFallback {
            return false;
        }
        if self.active_content_surface_for_layout_action().is_none() {
            return false;
        }
        let axis = self.smart_split_axis_for_active_leaf();
        self.split_active_content_leaf(axis)
    }

    fn active_content_surface_for_layout_action(&self) -> Option<WlSurface> {
        self.active_toplevel_surface
            .as_ref()
            .filter(|surface| self.surface_has_visible_pane_item(surface))
            .cloned()
            .or_else(|| self.surface_for_layout_leaf(self.layout.active_leaf()))
    }

    fn smart_split_axis_for_active_leaf(&self) -> LayoutAxis {
        let rect = self
            .layout
            .active_rect(self.presentation_layout_size())
            .unwrap_or_else(|| fullscreen_layout_rect(self.presentation_layout_size()));
        if rect.w >= rect.h {
            LayoutAxis::Horizontal
        } else {
            LayoutAxis::Vertical
        }
    }

    fn focus_layout_leaf_by_offset(&mut self, offset: isize) -> bool {
        if self.resolved_layout_source != ResolvedLayoutSource::NativeFallback {
            return false;
        }
        let previous_leaf = self.layout.active_leaf();
        let Some(next_leaf) = self.layout.focus_leaf_by_offset(offset) else {
            return false;
        };
        if next_leaf == previous_leaf {
            return false;
        }
        self.refresh_resolved_layout_from_native_policy();
        if let Some(next_surface) = self.surface_for_layout_leaf(next_leaf) {
            self.focus_toplevel_surface(Some(next_surface));
        } else {
            self.focus_toplevel_surface(None);
            self.reconfigure_focus_stack();
        }
        true
    }

    fn remove_active_split(&mut self) -> bool {
        if self.resolved_layout_source != ResolvedLayoutSource::NativeFallback {
            return false;
        }
        let Some((removed_leaf, next_leaf)) = self.layout.remove_active_leaf() else {
            return false;
        };
        self.layout_leaf_selected_panes
            .retain(|(leaf, _)| *leaf != removed_leaf);
        self.refresh_resolved_layout_from_native_policy();
        if let Some(next_surface) = self.surface_for_layout_leaf(next_leaf) {
            self.focus_toplevel_surface(Some(next_surface));
        } else {
            self.focus_toplevel_surface(None);
            self.reconfigure_focus_stack();
        }
        true
    }

    fn selected_panes_for_other_layout_leaves(&self, active_leaf: LayoutLeafId) -> Vec<u64> {
        self.layout_leaf_selected_panes
            .iter()
            .filter_map(|(leaf, pane)| (*leaf != active_leaf).then_some(*pane))
            .filter(|pane| {
                self.active_session_content_surface_for_pane(*pane)
                    .is_some()
            })
            .collect()
    }

    pub fn focused_window_index(&self) -> Option<usize> {
        let active = self.active_toplevel_surface.as_ref()?;
        if !self.surface_can_receive_focus(active) {
            return None;
        }
        self.space
            .elements()
            .filter(|window| self.window_visible_in_presentation(window))
            .position(|window| Self::window_toplevel_surface(window).as_ref() == Some(active))
    }

    pub fn focused_window_count(&self) -> usize {
        usize::from(self.active_window().is_some())
    }

    pub fn focused_surface_id(&self) -> Option<String> {
        let surface = self.active_toplevel_surface.as_ref()?;
        if !self.surface_can_receive_focus(surface) {
            return None;
        }
        Some(crate::presentation::sanitize_status_value(&format!(
            "{:?}",
            surface.id()
        )))
    }

    pub fn reconfigure_focus_stack(&mut self) {
        let windows = self
            .space
            .elements()
            .filter(|window| self.window_belongs_to_active_surface_session(window))
            .cloned()
            .collect::<Vec<_>>();
        let visible_windows = windows
            .iter()
            .filter(|window| self.window_visible_in_presentation(window))
            .cloned()
            .collect::<Vec<_>>();
        for window in &windows {
            let location = self.window_layout_rect(window).map_or_else(
                || focus_stack_location(0),
                |rect| Point::from((rect.x, rect.y)),
            );
            self.space.map_element(window.clone(), location, false);
        }
        if self.resolved_layout_source == ResolvedLayoutSource::NativeFallback
            && self.layout.active_kind().is_some()
            && self
                .surface_for_layout_leaf(self.layout.active_leaf())
                .is_none()
        {
            self.focus_toplevel_surface(None);
            return;
        }
        if self.active_window().is_none() {
            let active_leaf_surface = (self.resolved_layout_source
                == ResolvedLayoutSource::NativeFallback)
                .then(|| self.surface_for_layout_leaf(self.layout.active_leaf()))
                .flatten()
                .filter(|surface| self.surface_has_visible_pane_item(surface));
            if active_leaf_surface.is_some() {
                self.focus_toplevel_surface(active_leaf_surface);
            }
        }
        if let Some(active_window) = self.active_window() {
            self.space.raise_element(&active_window, true);
            self.activate_window_exclusively(self.active_toplevel_surface.clone().as_ref());
        } else if let Some(window) = visible_windows.last() {
            self.focus_window(window);
        } else {
            self.focus_toplevel_surface(None);
        }
    }

    pub const fn toggle_input_mode(&mut self) -> InputMode {
        self.input_mode = match self.input_mode {
            InputMode::Passthrough => InputMode::Normal,
            InputMode::Normal => InputMode::Passthrough,
        };
        self.input_mode
    }

    pub fn toggle_managed_fullscreen(&mut self) -> bool {
        self.managed_fullscreen = !self.managed_fullscreen;
        self.reconfigure_focus_stack();
        self.managed_fullscreen
    }

    pub const fn managed_fullscreen(&self) -> bool {
        self.managed_fullscreen
    }

    pub fn apply_layout_action(&mut self, action: LayoutAction) -> bool {
        match action {
            LayoutAction::FocusNext => {
                self.resolved_layout_source == ResolvedLayoutSource::NativeFallback
                    && self.cycle_focus_in_active_layout_leaf(FocusDirection::Next)
            }
            LayoutAction::FocusPrevious => {
                self.resolved_layout_source == ResolvedLayoutSource::NativeFallback
                    && self.cycle_focus_in_active_layout_leaf(FocusDirection::Previous)
            }
            LayoutAction::CloseActivePane => self.close_active_window(),
            LayoutAction::ToggleManagedFullscreen => {
                self.toggle_managed_fullscreen();
                true
            }
            LayoutAction::SplitActiveRight => {
                self.split_active_content_leaf(LayoutAxis::Horizontal)
            }
            LayoutAction::SplitActiveDown => self.split_active_content_leaf(LayoutAxis::Vertical),
            LayoutAction::JoinActivePane => self.remove_active_split(),
            LayoutAction::SplitActiveSmart => self.split_active_content_leaf_smart(),
            LayoutAction::FocusNextSplit => self.focus_layout_leaf_by_offset(1),
            LayoutAction::FocusPreviousSplit => self.focus_layout_leaf_by_offset(-1),
            LayoutAction::MoveActiveToLeftDock
            | LayoutAction::MoveActiveToRightDock
            | LayoutAction::MoveActiveToBottomDock
            | LayoutAction::MoveActiveToCenter => {
                warn!(
                    ?action,
                    "layout action accepted but not implemented by the hardcoded layout yet"
                );
                false
            }
            LayoutAction::ToggleLeftDock => {
                self.toggle_left_panel_visibility();
                self.reconfigure_focus_stack();
                true
            }
            LayoutAction::ToggleRightDock => {
                self.layout.right_dock_visible = !self.layout.right_dock_visible;
                true
            }
            LayoutAction::ToggleBottomDock => {
                self.layout.bottom_dock_visible = !self.layout.bottom_dock_visible;
                true
            }
            LayoutAction::IncreaseLeftDockWidth => {
                self.adjust_left_panel_width(LEFT_PANEL_KEYBOARD_STEP);
                self.reconfigure_focus_stack();
                true
            }
            LayoutAction::DecreaseLeftDockWidth => {
                self.adjust_left_panel_width(-LEFT_PANEL_KEYBOARD_STEP);
                self.reconfigure_focus_stack();
                true
            }
        }
    }

    pub fn close_active_window(&mut self) -> bool {
        let Some(window) = self.active_window() else {
            return false;
        };
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        self.reset_transient_input_state();
        toplevel.send_close();
        if let Err(err) = self.display_handle.flush_clients() {
            warn!(?err, "flush clients after close request failed");
        }
        true
    }

    pub fn spawn_wezterm_pane(&mut self) -> Result<()> {
        self.spawn_hosted_app_argv(&[
            "flatpak".to_string(),
            "run".to_string(),
            "--env=GDK_BACKEND=wayland".to_string(),
            "--env=QT_QPA_PLATFORM=wayland".to_string(),
            "org.wezfurlong.wezterm".to_string(),
        ])
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
        self.window_surface_sessions
            .retain(|(surface, _)| live_toplevel_surfaces.iter().any(|live| live == surface));
        self.presentation_child_surfaces
            .retain(|surface| live_toplevel_surfaces.iter().any(|live| live == surface));
        self.presentation_member_surfaces
            .retain(|surface| live_toplevel_surfaces.iter().any(|live| live == surface));
        self.panel_reservations.retain(|panel| {
            live_toplevel_surfaces
                .iter()
                .any(|live| live == &panel.surface)
        });
        let closed_panes = self
            .window_pane_ids
            .iter()
            .filter(|(surface, _)| !live_toplevel_surfaces.iter().any(|live| live == surface))
            .map(|(_, pane)| *pane)
            .collect::<Vec<_>>();
        for pane_id in closed_panes {
            for agent_id in self.unregister_agents_for_pane(pane_id) {
                info!(
                    pane_id,
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
        self.repair_resolved_layout_for_current_inventory();
        if self
            .pending_pointer_focus_surface
            .as_ref()
            .is_some_and(|surface| !live_toplevel_surfaces.iter().any(|live| live == surface))
        {
            self.pending_pointer_focus_surface = None;
        }
        if self
            .pointer_hover_toplevel_surface
            .as_ref()
            .is_some_and(|surface| !live_toplevel_surfaces.iter().any(|live| live == surface))
        {
            self.pointer_hover_toplevel_surface = None;
        }
        let window_count = self.active_surface_session_window_count();
        let active_surface_is_live = self
            .active_toplevel_surface
            .as_ref()
            .is_some_and(|surface| {
                self.surface_is_live_focus_target(surface, &live_toplevel_surfaces)
            });

        if self.active_toplevel_surface.is_some() && !active_surface_is_live {
            self.reset_transient_input_state();
            if let Some(next_focus) = live_toplevel_surfaces
                .iter()
                .find(|surface| {
                    self.session_for_toplevel_surface(surface)
                        .unwrap_or(SurfaceSessionId(1))
                        == self.active_surface_session
                })
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
                    .find(|surface| {
                        self.session_for_toplevel_surface(surface)
                            .unwrap_or(SurfaceSessionId(1))
                            == self.active_surface_session
                    })
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
            && (self
                .session_for_toplevel_surface(surface)
                .unwrap_or(SurfaceSessionId(1))
                == self.active_surface_session
                || self.surface_is_visible_presentation_member(surface)
                || self.surface_has_visible_panel(surface))
    }

    fn repair_resolved_layout_for_current_inventory(&mut self) {
        if self.resolved_layout_source == ResolvedLayoutSource::NativeFallback {
            self.collapse_empty_native_layout_leaves();
            self.refresh_resolved_layout_from_native_policy();
            return;
        }

        let valid_panes = self.active_surface_session_panes();
        if let [pane] = valid_panes.as_slice() {
            self.accepted_resolved_layout = self.single_pane_resolved_layout(*pane);
            return;
        }

        let mut layout = self.accepted_resolved_layout.clone();
        layout.items.retain(|item| match item.kind {
            LayoutItemKind::Pane { pane } => valid_panes.contains(&pane),
            LayoutItemKind::Placeholder
            | LayoutItemKind::Handle
            | LayoutItemKind::Chrome
            | LayoutItemKind::Overlay => true,
        });
        match layout.validate_for_panes(&valid_panes) {
            Ok(layout) => {
                self.accepted_resolved_layout = layout;
            }
            Err(err) => {
                warn!(
                    ?err,
                    "accepted WASM resolved layout became invalid; using native fallback"
                );
                self.refresh_resolved_layout_from_native_policy();
            }
        }
    }

    fn collapse_empty_native_layout_leaves(&mut self) {
        loop {
            let empty_leaf = self.layout.leaf_ids().into_iter().find(|leaf| {
                self.selected_pane_for_layout_leaf(*leaf)
                    .is_none_or(|pane| self.active_session_content_surface_for_pane(pane).is_none())
            });
            let Some(empty_leaf) = empty_leaf else {
                return;
            };
            if self.layout.remove_leaf(empty_leaf).is_none() {
                return;
            }
            self.layout_leaf_selected_panes
                .retain(|(leaf, _)| *leaf != empty_leaf);
        }
    }

    fn single_pane_resolved_layout(&self, pane: u64) -> ResolvedLayout {
        ResolvedLayout {
            items: vec![ResolvedLayoutItem {
                id: LayoutItemId(pane),
                kind: LayoutItemKind::Pane { pane },
                z: 0.0,
                transform: LocalTransform {
                    offset: LocalPoint { u: -1.0, v: -1.0 },
                    size: LocalPoint { u: 2.0, v: 2.0 },
                },
                input: InputBehavior::Pane,
            }],
        }
    }

    fn presentation_size(&self) -> (i32, i32) {
        self.space
            .outputs()
            .next()
            .and_then(|output| self.space.output_geometry(output))
            .map_or((1280, 800), |geometry| {
                (geometry.size.w.max(1), geometry.size.h.max(1))
            })
    }

    pub fn presentation_layout_size(&self) -> LayoutSize {
        let (w, h) = self.presentation_size();
        let left = self.left_panel_reserved_width();
        LayoutSize {
            w: (w - left).max(1),
            h,
        }
    }

    pub fn active_layout_rect(&self) -> Option<LayoutRect> {
        if let Some(surface) = self.active_toplevel_surface.as_ref()
            && let Some(rect) = self.layout_rect_for_toplevel_surface(surface)
        {
            return Some(rect);
        }
        if self.resolved_layout_source == ResolvedLayoutSource::NativeFallback {
            return self
                .layout
                .active_rect(self.presentation_layout_size())
                .map(|rect| self.translate_content_rect(rect));
        }
        None
    }

    pub const fn resolved_layout_source_name(&self) -> &'static str {
        match self.resolved_layout_source {
            ResolvedLayoutSource::NativeFallback => "native-fallback",
            ResolvedLayoutSource::WasmPolicy => "wasm-policy",
        }
    }

    pub const fn resolved_layout_item_count(&self) -> usize {
        self.accepted_resolved_layout.items.len()
    }

    pub fn resolved_layout_pane_count(&self) -> usize {
        self.accepted_resolved_layout.pane_count()
    }

    pub fn resolved_layout_placeholder_count(&self) -> usize {
        self.accepted_resolved_layout.placeholder_count()
    }

    pub fn resolved_layout_active_kind(&self) -> &'static str {
        if self
            .active_toplevel_surface
            .as_ref()
            .is_some_and(|surface| self.surface_has_visible_pane_item(surface))
        {
            return "client";
        }
        if self
            .active_toplevel_surface
            .as_ref()
            .is_some_and(|surface| {
                self.surface_is_visible_presentation_member(surface)
                    || self.surface_has_visible_panel(surface)
            })
        {
            return "presentation";
        }
        if self.resolved_layout_source == ResolvedLayoutSource::NativeFallback {
            return match self.layout.active_kind() {
                Some(LayoutLeafKind::Client)
                    if self
                        .surface_for_layout_leaf(self.layout.active_leaf())
                        .is_some() =>
                {
                    "client"
                }
                Some(LayoutLeafKind::Client) => "placeholder",
                None => "none",
            };
        }
        "none"
    }

    pub fn resolved_layout_items(&self) -> &[ResolvedLayoutItem] {
        &self.accepted_resolved_layout.items
    }

    pub fn resolved_layout_item_rect(&self, item: ResolvedLayoutItem) -> LayoutRect {
        self.translate_content_rect(item.pixel_rect(self.presentation_layout_size()))
    }

    pub fn resolved_layout_item_active(&self, item: ResolvedLayoutItem) -> bool {
        match item.kind {
            LayoutItemKind::Pane { pane } => {
                self.active_toplevel_surface
                    .as_ref()
                    .and_then(|surface| self.pane_for_toplevel_surface(surface))
                    == Some(pane)
            }
            LayoutItemKind::Placeholder => {
                self.resolved_layout_source == ResolvedLayoutSource::NativeFallback
                    && item.id == LayoutItemId(1_000_000 + self.layout.active_leaf().raw())
            }
            LayoutItemKind::Handle | LayoutItemKind::Chrome | LayoutItemKind::Overlay => false,
        }
    }

    fn translate_content_rect(&self, rect: LayoutRect) -> LayoutRect {
        LayoutRect {
            x: rect.x + self.left_panel_reserved_width(),
            ..rect
        }
    }

    fn content_rect(&self) -> LayoutRect {
        let size = self.presentation_layout_size();
        LayoutRect {
            x: self.left_panel_reserved_width(),
            y: 0,
            w: size.w,
            h: size.h,
        }
    }

    fn panel_rect_for_surface(&self, surface: &WlSurface) -> Option<LayoutRect> {
        let panel = self
            .panel_reservations
            .iter()
            .rev()
            .find(|panel| panel.visible && panel.surface == *surface)?;
        let (_, h) = self.presentation_size();
        match panel.edge {
            PanelEdge::Left => Some(LayoutRect {
                x: 0,
                y: 0,
                w: panel.width,
                h,
            }),
        }
    }

    pub fn left_panel_reserved_width(&self) -> i32 {
        self.panel_reservations
            .iter()
            .rev()
            .find(|panel| panel.edge == PanelEdge::Left && panel.visible)
            .map_or(0, |panel| panel.width)
    }

    pub fn left_panel_splitter_hit(&self, x: f64, y: f64) -> bool {
        let width = f64::from(self.left_panel_reserved_width());
        if width <= 0.0 {
            return false;
        }
        let (_, h) = self.presentation_size();
        y >= 0.0 && y < f64::from(h) && (x - width).abs() <= LEFT_PANEL_SPLITTER_HITBOX
    }

    pub fn begin_left_panel_resize(&mut self) {
        self.left_panel_resizing = self.left_panel_reserved_width() > 0;
    }

    pub const fn end_left_panel_resize(&mut self) -> bool {
        let was_resizing = self.left_panel_resizing;
        self.left_panel_resizing = false;
        was_resizing
    }

    pub fn resize_left_panel_to_pointer(&mut self, x: f64) -> bool {
        if !self.left_panel_resizing {
            return false;
        }
        let target = x.round() as i32;
        self.set_left_panel_width(target);
        true
    }

    fn toggle_left_panel_visibility(&mut self) {
        self.layout.left_dock_visible = !self.layout.left_dock_visible;
        for panel in self
            .panel_reservations
            .iter_mut()
            .filter(|panel| panel.edge == PanelEdge::Left)
        {
            panel.visible = self.layout.left_dock_visible;
        }
    }

    fn adjust_left_panel_width(&mut self, delta: i32) {
        let width = self
            .panel_reservations
            .iter()
            .rev()
            .find(|panel| panel.edge == PanelEdge::Left)
            .map_or(LEFT_PANEL_DEFAULT_WIDTH, |panel| panel.width);
        self.set_left_panel_width(width + delta);
    }

    fn set_left_panel_width(&mut self, width: i32) {
        let (presentation_w, _) = self.presentation_size();
        let max_width = LEFT_PANEL_MAX_WIDTH.min((presentation_w - 120).max(LEFT_PANEL_MIN_WIDTH));
        let width = width.clamp(LEFT_PANEL_MIN_WIDTH, max_width);
        for panel in self
            .panel_reservations
            .iter_mut()
            .filter(|panel| panel.edge == PanelEdge::Left)
        {
            panel.width = width;
        }
    }
}

fn default_surface_session_name(session: SurfaceSessionId) -> String {
    let n = session.raw().saturating_sub(1) as usize;
    let adjective = SESSION_NAME_ADJECTIVES[n % SESSION_NAME_ADJECTIVES.len()];
    let noun = SESSION_NAME_NOUNS[(n / SESSION_NAME_ADJECTIVES.len()) % SESSION_NAME_NOUNS.len()];
    let action = SESSION_NAME_ACTIONS[(n
        / (SESSION_NAME_ADJECTIVES.len() * SESSION_NAME_NOUNS.len()))
        % SESSION_NAME_ACTIONS.len()];
    format!("{adjective}-{noun}-{action}")
}

fn fallback_marker_id(index: usize) -> u64 {
    1_000_000 + u64::try_from(index).unwrap_or(u64::MAX - 1_000_000)
}

fn current_process_directory() -> PathBuf {
    let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    directory.canonicalize().unwrap_or(directory)
}

fn fullscreen_layout_rect(size: LayoutSize) -> LayoutRect {
    LayoutRect {
        x: 0,
        y: 0,
        w: size.w.max(1),
        h: size.h.max(1),
    }
}

fn first_available_pane_from(
    panes: &[u64],
    start: u64,
    excluded: &[u64],
    step: isize,
) -> Option<u64> {
    if panes.is_empty() {
        return None;
    }
    let Some(start_index) = panes.iter().position(|pane| *pane == start) else {
        return panes.iter().copied().find(|pane| !excluded.contains(pane));
    };
    let len = panes.len() as isize;
    let step = match step.cmp(&0) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    for offset in 0..panes.len() {
        let offset = offset as isize;
        let index = if step == 0 {
            (start_index as isize + offset).rem_euclid(len)
        } else {
            (start_index as isize + (offset + 1) * step).rem_euclid(len)
        } as usize;
        let pane = panes[index];
        if !excluded.contains(&pane) {
            return Some(pane);
        }
    }
    None
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
        if entry_key == key.as_bytes() && entry_value.first() == Some(&b'=') {
            Some(String::from_utf8_lossy(&entry_value[1..]).into_owned())
        } else {
            None
        }
    })
}

fn placement_requests_left_panel(placement: &ExtensionMetadata) -> bool {
    let edge = placement
        .get("edge")
        .or_else(|| placement.get("side"))
        .is_some_and(|value| value.eq_ignore_ascii_case("left"));
    let panel_mode = placement
        .get("mode")
        .or_else(|| placement.get("kind"))
        .or_else(|| placement.get("placement"))
        .is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "panel" | "sidebar" | "dock" | "left-panel" | "left-sidebar"
            )
        });
    edge || panel_mode
}

fn placement_panel_width(placement: &ExtensionMetadata) -> Option<i32> {
    placement
        .get("width")
        .or_else(|| placement.get("size"))
        .and_then(|value| value.parse().ok())
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

#[derive(Default)]
struct WindowRefreshOutcome {
    focus_cleared: bool,
    focus_repaired: bool,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "unit tests use direct fixture setup and focused state assertions"
)]
mod tests {
    use super::{
        Manifold, PresentationStatus, SESSION_OUTPUT_SIZE, fullscreen_layout_rect,
        window_surface_origin_in_compositor_space,
    };
    use crate::layout::LayoutSize;
    use smithay::{
        reexports::{calloop::EventLoop, wayland_server::Display},
        utils::Point,
    };
    use std::{collections::BTreeMap, path::PathBuf};

    #[test]
    fn fullscreen_layout_covers_the_presentation_from_origin() {
        let rect = fullscreen_layout_rect(LayoutSize { w: 1920, h: 1080 });

        assert_eq!((rect.x, rect.y, rect.w, rect.h), (0, 0, 1920, 1080));
    }

    #[test]
    fn fullscreen_layout_clamps_empty_presentation_size() {
        let rect = fullscreen_layout_rect(LayoutSize { w: 0, h: -4 });

        assert_eq!((rect.x, rect.y, rect.w, rect.h), (0, 0, 1, 1));
    }

    #[test]
    fn surface_origin_accounts_for_window_geometry_offset() {
        let window_location = Point::from((0, 0));
        let window_geometry_location = Point::from((4, 28));
        let origin =
            window_surface_origin_in_compositor_space(window_location, window_geometry_location);

        assert_eq!((origin.x, origin.y), (-4, -28));
    }

    #[test]
    fn detached_session_starts_with_mapped_output() {
        let mut event_loop = EventLoop::try_new().expect("event loop");
        let display = Display::new().expect("display");
        let state = Manifold::new(&mut event_loop, display);

        assert_eq!(state.presentation, PresentationStatus::Detached);
        assert_eq!(state.space.outputs().count(), 1);
        let geometry = state
            .space
            .output_geometry(&state.session_output)
            .expect("session output geometry");
        assert_eq!(
            (geometry.size.w, geometry.size.h),
            (SESSION_OUTPUT_SIZE.0, SESSION_OUTPUT_SIZE.1)
        );
    }

    #[test]
    fn directory_created_session_carries_the_directory_as_session_cwd() {
        let mut event_loop = EventLoop::try_new().expect("event loop");
        let display = Display::new().expect("display");
        let mut state = Manifold::new(&mut event_loop, display);
        let directory = PathBuf::from("/tmp/manifold-session-cwd/subdir");

        let session = state.create_and_attach_named_surface_session_in_directory(
            Some("subdir".to_string()),
            directory.clone(),
        );
        assert!(state.remember_directory_surface_session(directory.clone(), session, true));

        assert_eq!(
            state.surface_session_working_directory(session),
            Some(directory.as_path())
        );
        assert_eq!(
            state.resolve_directory_surface_session(&directory),
            Some(session)
        );
        assert_eq!(
            state
                .surface_session_summary(session)
                .and_then(|summary| summary.working_directory),
            Some(directory.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn first_session_after_clearing_initial_surface_session_reuses_session_one() {
        let mut event_loop = EventLoop::try_new().expect("event loop");
        let display = Display::new().expect("display");
        let mut state = Manifold::new(&mut event_loop, display);
        let directory = PathBuf::from("/tmp/manifold-session-cwd/first");

        state.clear_initial_surface_session();
        assert_eq!(state.surface_session_count(), 0);

        let session =
            state.create_and_attach_named_surface_session_in_directory(None, directory.clone());
        assert!(state.remember_directory_surface_session(directory.clone(), session, true));

        assert_eq!(session.raw(), 1);
        assert_eq!(state.active_surface_session().raw(), 1);
        assert_eq!(state.surface_session_count(), 1);
        assert_eq!(
            state.resolve_directory_surface_session(&directory),
            Some(session)
        );
    }

    #[test]
    fn remembering_directory_does_not_rewrite_existing_session_cwd() {
        let mut event_loop = EventLoop::try_new().expect("event loop");
        let display = Display::new().expect("display");
        let mut state = Manifold::new(&mut event_loop, display);
        let original = PathBuf::from("/tmp/manifold-session-cwd/original");
        let alias = PathBuf::from("/tmp/manifold-session-cwd/alias");

        let session =
            state.create_and_attach_named_surface_session_in_directory(None, original.clone());
        assert!(state.remember_directory_surface_session(alias, session, true));

        assert_eq!(
            state.surface_session_working_directory(session),
            Some(original.as_path())
        );
    }

    #[test]
    fn agent_desktop_notification_still_sends_when_presentation_focused_but_agent_is_not() {
        let mut event_loop = EventLoop::try_new().expect("event loop");
        let display = Display::new().expect("display");
        let mut state = Manifold::new(&mut event_loop, display);

        let mut metadata = BTreeMap::new();
        metadata.insert("app_id".to_string(), "smoke-agent".to_string());
        let agent_id = state
            .register_agent("codex".to_string(), metadata, ":test.sender".to_string())
            .expect("register agent");

        assert!(state.should_notify_agent_desktop(&agent_id));
        state.set_presentation_window_focused(true);
        assert!(state.should_notify_agent_desktop(&agent_id));
    }

    #[test]
    fn presentation_focus_loss_clears_transient_keyboard_state() {
        let mut event_loop = EventLoop::try_new().expect("event loop");
        let display = Display::new().expect("display");
        let mut state = Manifold::new(&mut event_loop, display);

        state.set_presentation_window_focused(true);
        state.ctrl_pressed = true;
        state.ctrl_pressed_key = Some(smithay::backend::input::Keycode::new(37));
        state.ctrl_forwarded_to_client = true;
        state
            .forwarded_pressed_keys
            .push(smithay::backend::input::Keycode::new(37));
        state.shortcut_prefix_active = true;
        state.suppress_shortcut_prefix_key_release =
            Some(smithay::backend::input::Keycode::new(56));

        state.set_presentation_window_focused(false);

        assert!(!state.ctrl_pressed);
        assert_eq!(state.ctrl_pressed_key, None);
        assert!(!state.ctrl_forwarded_to_client);
        assert!(state.forwarded_pressed_keys.is_empty());
        assert!(!state.shortcut_prefix_active);
        assert_eq!(state.suppress_shortcut_prefix_key_release, None);
    }

    #[test]
    fn presentation_focus_loss_releases_untracked_forwarded_keys() {
        let mut event_loop = EventLoop::try_new().expect("event loop");
        let display = Display::new().expect("display");
        let mut state = Manifold::new(&mut event_loop, display);

        state.set_presentation_window_focused(true);
        state.forward_keyboard_key(
            smithay::backend::input::Keycode::new(133),
            smithay::backend::input::KeyState::Pressed,
            1,
        );
        assert_eq!(
            state.forwarded_pressed_keys,
            vec![smithay::backend::input::Keycode::new(133)]
        );

        state.set_presentation_window_focused(false);

        assert!(state.forwarded_pressed_keys.is_empty());
    }

    #[test]
    fn agent_registration_uses_pane_id_as_stable_identity() {
        let mut event_loop = EventLoop::try_new().expect("event loop");
        let display = Display::new().expect("display");
        let mut state = Manifold::new(&mut event_loop, display);

        let mut first = BTreeMap::new();
        first.insert("pane_id".to_string(), "7".to_string());
        first.insert("status".to_string(), "running".to_string());
        first.insert("cwd".to_string(), "/tmp/first".to_string());
        first.insert("first_user_message".to_string(), "first prompt".to_string());
        let first_id = state
            .register_agent("codex".to_string(), first, ":test.sender".to_string())
            .expect("register first agent state");

        let mut second = BTreeMap::new();
        second.insert("pane_id".to_string(), "7".to_string());
        second.insert("status".to_string(), "done".to_string());
        second.insert("cwd".to_string(), "/tmp/second".to_string());
        second.insert(
            "first_user_message".to_string(),
            "second prompt".to_string(),
        );
        let second_id = state
            .register_agent("codex".to_string(), second, ":test.sender".to_string())
            .expect("register second agent state");

        let agents = state.agent_summaries();
        assert_eq!(first_id, second_id);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, "done");
        assert_eq!(
            agents[0].metadata.get("cwd").map(String::as_str),
            Some("/tmp/second")
        );
        assert_eq!(
            agents[0]
                .metadata
                .get("first_user_message")
                .map(String::as_str),
            Some("first prompt")
        );
    }
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
    pub attach_count: u64,
    pub detach_count: u64,
    pub pending_attach_request_at: Option<Instant>,
    pub pending_attach_frame_at: Option<Instant>,
    pub pending_detach_request_at: Option<Instant>,
    pub last_attach_latency_micros: Option<u128>,
    pub max_attach_latency_micros: u128,
    pub last_attach_to_frame_micros: Option<u128>,
    pub max_attach_to_frame_micros: u128,
    pub last_detach_latency_micros: Option<u128>,
    pub max_detach_latency_micros: u128,
}

impl RuntimeStats {
    pub fn record_attach_request(&mut self) {
        if self.pending_attach_request_at.is_none() {
            self.pending_attach_request_at = Some(Instant::now());
        }
        if self.pending_attach_frame_at.is_none() {
            self.pending_attach_frame_at = Some(Instant::now());
        }
    }

    pub fn record_detach_request(&mut self) {
        if self.pending_detach_request_at.is_none() {
            self.pending_detach_request_at = Some(Instant::now());
        }
    }

    pub fn record_attach_applied(&mut self) {
        if let Some(requested_at) = self.pending_attach_request_at.take() {
            let latency = requested_at.elapsed().as_micros();
            self.last_attach_latency_micros = Some(latency);
            self.max_attach_latency_micros = self.max_attach_latency_micros.max(latency);
        }
    }

    pub const fn record_attach_failed(&mut self) {
        self.pending_attach_request_at = None;
        self.pending_attach_frame_at = None;
    }

    pub fn record_detach_applied(&mut self) {
        if let Some(requested_at) = self.pending_detach_request_at.take() {
            let latency = requested_at.elapsed().as_micros();
            self.last_detach_latency_micros = Some(latency);
            self.max_detach_latency_micros = self.max_detach_latency_micros.max(latency);
        }
        self.clear_pending_visible_latency_samples();
    }

    const fn clear_pending_visible_latency_samples(&mut self) {
        self.pending_input_at = None;
        self.pending_input_events = 0;
        self.pending_input_commit_at = None;
        self.pending_input_to_commit_micros = None;
        self.pending_commit_at = None;
        self.pending_commits = 0;
        self.pending_client_frame_callback_at = None;
        self.pending_client_frame_callbacks = 0;
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

    pub fn input_frame_defer_remaining(&self, max_delay: Duration) -> Option<Duration> {
        let input_at = self.pending_input_at?;
        if self.pending_input_commit_at.is_some() {
            return None;
        }

        max_delay.checked_sub(input_at.elapsed())
    }

    pub const fn record_input_frame_deferred(&mut self) {
        self.input_frame_deferrals += 1;
    }

    pub const fn record_input_deferred_frame(&mut self) {
        self.input_deferred_frames += 1;
    }

    pub const fn record_client_frame_deferred(&mut self) {
        self.client_frame_deferrals += 1;
    }

    pub const fn record_client_deferred_frame(&mut self) {
        self.client_deferred_frames += 1;
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

        if let Some(attach_at) = self.pending_attach_frame_at.take() {
            let latency = attach_at.elapsed().as_micros();
            self.last_attach_to_frame_micros = Some(latency);
            self.max_attach_to_frame_micros = self.max_attach_to_frame_micros.max(latency);
        }
    }
}

#[cfg(test)]
mod runtime_stats_tests {
    use std::time::Duration;

    use super::RuntimeStats;

    #[test]
    fn input_latency_batches_events_until_next_frame() {
        let mut stats = RuntimeStats::default();

        stats.record_input(true);
        stats.record_input(true);
        stats.record_frame(Duration::ZERO);

        assert_eq!(stats.input_events, 2);
        assert_eq!(stats.input_frame_count, 1);
        assert_eq!(stats.last_input_batch_events, 2);
        assert_eq!(stats.max_input_batch_events, 2);
        assert_eq!(stats.pending_input_events, 0);
        assert!(stats.last_input_to_frame_micros.is_some());
        assert!(stats.last_input_to_submit_micros.is_some());
        assert_eq!(stats.input_without_commit_frame_count, 1);
    }

    #[test]
    fn idle_frames_do_not_clear_last_input_batch() {
        let mut stats = RuntimeStats::default();

        stats.record_input(true);
        stats.record_frame(Duration::ZERO);
        stats.record_frame(Duration::ZERO);

        assert_eq!(stats.input_frame_count, 1);
        assert_eq!(stats.last_input_batch_events, 1);
        assert_eq!(stats.max_input_batch_events, 1);
    }

    #[test]
    fn detached_input_counts_without_visible_latency() {
        let mut stats = RuntimeStats::default();

        stats.record_input(false);
        stats.record_frame(Duration::ZERO);

        assert_eq!(stats.input_events, 1);
        assert_eq!(stats.input_frame_count, 0);
        assert_eq!(stats.pending_input_events, 0);
        assert!(stats.last_input_to_frame_micros.is_none());
    }

    #[test]
    fn commit_latency_batches_visible_commits_until_next_frame() {
        let mut stats = RuntimeStats::default();

        stats.record_commit(true);
        stats.record_commit(true);
        stats.record_commit(false);
        stats.record_frame(Duration::ZERO);

        assert_eq!(stats.commits, 3);
        assert_eq!(stats.commit_frame_count, 1);
        assert_eq!(stats.last_commit_batch_commits, 2);
        assert_eq!(stats.max_commit_batch_commits, 2);
        assert_eq!(stats.pending_commits, 0);
        assert!(stats.last_commit_to_frame_micros.is_some());
        assert!(stats.last_commit_to_submit_micros.is_some());
    }

    #[test]
    fn input_followed_by_commit_before_frame_records_same_frame_candidate() {
        let mut stats = RuntimeStats::default();

        stats.record_input(true);
        stats.record_commit(true);
        stats.record_frame(Duration::ZERO);

        assert_eq!(stats.input_frame_count, 1);
        assert_eq!(stats.commit_frame_count, 1);
        assert_eq!(stats.input_commit_frame_count, 1);
        assert_eq!(stats.input_without_commit_frame_count, 0);
        assert!(stats.last_input_to_commit_micros.is_some());
        assert!(stats.last_input_commit_to_submit_micros.is_some());
        assert!(stats.max_input_to_commit_micros >= stats.last_input_to_commit_micros.unwrap());
        assert!(
            stats.max_input_commit_to_submit_micros
                >= stats.last_input_commit_to_submit_micros.unwrap()
        );
    }

    #[test]
    fn input_frame_defer_remaining_only_waits_for_input_without_commit() {
        let mut stats = RuntimeStats::default();

        assert!(
            stats
                .input_frame_defer_remaining(Duration::from_millis(3))
                .is_none()
        );

        stats.record_input(true);
        assert!(
            stats
                .input_frame_defer_remaining(Duration::from_millis(3))
                .is_some()
        );

        stats.record_commit(true);
        assert!(
            stats
                .input_frame_defer_remaining(Duration::from_millis(3))
                .is_none()
        );
    }

    #[test]
    fn commit_before_input_is_not_an_input_commit_hit() {
        let mut stats = RuntimeStats::default();

        stats.record_commit(true);
        stats.record_input(true);
        stats.record_frame(Duration::ZERO);

        assert_eq!(stats.input_frame_count, 1);
        assert_eq!(stats.commit_frame_count, 1);
        assert_eq!(stats.input_commit_frame_count, 0);
        assert_eq!(stats.input_without_commit_frame_count, 1);
        assert!(stats.last_input_to_commit_micros.is_none());
        assert!(stats.last_input_commit_to_submit_micros.is_none());
    }

    #[test]
    fn client_frame_callback_to_commit_latency_uses_first_following_commit() {
        let mut stats = RuntimeStats::default();

        stats.record_client_frame_callbacks(1);
        stats.record_client_frame_callbacks(2);
        stats.record_commit(true);
        stats.record_commit(true);

        assert_eq!(stats.client_frame_callbacks, 3);
        assert_eq!(stats.client_frame_callback_commit_count, 1);
        assert_eq!(stats.last_client_frame_callback_batch_callbacks, 3);
        assert_eq!(stats.max_client_frame_callback_batch_callbacks, 3);
        assert_eq!(stats.pending_client_frame_callbacks, 0);
        assert!(stats.pending_client_frame_callback_at.is_none());
        assert!(stats.last_client_frame_callback_to_commit_micros.is_some());
        assert!(
            stats.max_client_frame_callback_to_commit_micros
                >= stats.last_client_frame_callback_to_commit_micros.unwrap()
        );
    }

    #[test]
    fn detached_commits_do_not_create_visible_latency() {
        let mut stats = RuntimeStats::default();

        stats.record_commit(false);
        stats.record_frame(Duration::ZERO);

        assert_eq!(stats.commits, 1);
        assert_eq!(stats.commit_frame_count, 0);
        assert_eq!(stats.pending_commits, 0);
        assert!(stats.last_commit_to_frame_micros.is_none());
    }

    #[test]
    fn detach_clears_pending_visible_latency_without_resetting_totals() {
        let mut stats = RuntimeStats::default();

        stats.record_input(true);
        stats.record_commit(true);
        stats.record_detach_request();
        stats.record_detach_applied();
        stats.record_frame(Duration::ZERO);

        assert_eq!(stats.input_events, 1);
        assert_eq!(stats.commits, 1);
        assert_eq!(stats.pending_input_events, 0);
        assert_eq!(stats.pending_commits, 0);
        assert_eq!(stats.pending_client_frame_callbacks, 0);
        assert_eq!(stats.input_frame_count, 0);
        assert_eq!(stats.commit_frame_count, 0);
        assert!(stats.last_input_to_frame_micros.is_none());
        assert!(stats.last_commit_to_frame_micros.is_none());
        assert!(stats.last_input_to_commit_micros.is_none());
        assert!(stats.last_input_commit_to_submit_micros.is_none());
        assert!(stats.last_detach_latency_micros.is_some());
    }

    #[test]
    fn presentation_request_latency_is_measured_when_applied() {
        let mut stats = RuntimeStats::default();

        stats.record_attach_request();
        stats.record_attach_applied();
        stats.record_frame(Duration::ZERO);
        stats.record_detach_request();
        stats.record_detach_applied();

        assert!(stats.last_attach_latency_micros.is_some());
        assert!(stats.max_attach_latency_micros >= stats.last_attach_latency_micros.unwrap());
        assert!(stats.last_attach_to_frame_micros.is_some());
        assert!(stats.max_attach_to_frame_micros >= stats.last_attach_to_frame_micros.unwrap());
        assert!(stats.last_detach_latency_micros.is_some());
        assert!(stats.max_detach_latency_micros >= stats.last_detach_latency_micros.unwrap());
    }

    #[test]
    fn failed_attach_clears_pending_latency_measurements() {
        let mut stats = RuntimeStats::default();

        stats.record_attach_request();
        stats.record_attach_failed();
        stats.record_frame(Duration::ZERO);

        assert!(stats.last_attach_latency_micros.is_none());
        assert!(stats.last_attach_to_frame_micros.is_none());
        assert_eq!(stats.max_attach_latency_micros, 0);
        assert_eq!(stats.max_attach_to_frame_micros, 0);
    }
}

#[allow(dead_code)]
pub struct DmabufRuntime {
    pub state: DmabufState,
    pub global: Option<DmabufGlobal>,
    pub default_feedback: Option<DmabufFeedback>,
    pub importer: Option<SharedPresenterRuntime>,
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
            default_feedback: None,
            importer: None,
            stats: DmabufStats::default(),
            shm_formats: 0,
            advertised_formats: 0,
            protocol_version: None,
            render_node: None,
            egl_wl_bind_status: "uninitialized".to_string(),
            disabled_reason: None,
            explicit_sync_state: None,
            explicit_sync_protocol_version: None,
            explicit_sync_disabled_reason: "winit_backend_no_drm_import_device",
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
