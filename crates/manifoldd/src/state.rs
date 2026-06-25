use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
#[cfg(feature = "shm-fallback")]
use smithay::wayland::shm::ShmState;
use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
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
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        dmabuf::{DmabufFeedback, DmabufGlobal, DmabufState},
        drm_syncobj::DrmSyncobjState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::{XdgShellState, XdgToplevelSurfaceData},
        socket::ListeningSocketSource,
    },
};
use tracing::{info, warn};

use crate::{
    MANIFOLD_SESSION_ENV, MANIFOLDD_DBUS_ADDRESS_ENV, MANIFOLDD_DBUS_DESTINATION_ENV,
    browser::{BrowserInstanceSummary, BrowserLaunchOptions, BrowserManager},
    layout::{
        LayoutAction, LayoutAxis, LayoutLeafId, LayoutLeafKind, LayoutRect, LayoutSize, LayoutState,
    },
    presentation::PresenterHandle,
    wasm_key::WasmKeyHandler,
    wasm_map::{MapHookInput, MapHookOutcome, MapHookResponse, MapLayoutPatch, WasmMapHook},
    wasm_test::{
        TestHarnessCompletion, TestHarnessOutcomeKind, TestHarnessRuntime, service_request,
    },
    winit_presenter::SharedPresenterRuntime,
};

#[cfg(feature = "shm-fallback")]
const MANDATORY_SHM_FORMATS: usize = 2;
const WINDOW_LIFECYCLE_REFRESH_INTERVAL: Duration = Duration::from_millis(50);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum InputMode {
    Passthrough,
    Normal,
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
}

pub type ExtensionMetadata = BTreeMap<String, String>;

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

#[derive(Default)]
struct ExtensionRegistry {
    extensions: Vec<ExtensionRecord>,
    windows: Vec<WindowRecord>,
    grants: Vec<GrantRecord>,
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
struct GrantRecord {
    id: String,
    extension_id: String,
    kind: String,
    detail: ExtensionMetadata,
    sender: String,
    active: bool,
}

struct SessionDbusBus {
    session: SurfaceSessionId,
    address: String,
    daemon_pid: u32,
    dir: PathBuf,
}

impl Drop for SessionDbusBus {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.daemon_pid.to_string())
            .status();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub struct Manifold {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Manifold>,
    pub loop_signal: LoopSignal,

    pub space: Space<Window>,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    #[cfg(feature = "shm-fallback")]
    pub shm_state: ShmState,
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Manifold>,
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
    pub attach_requested: bool,
    pub attach_schedule_pending: bool,
    pub detach_requested: bool,
    pub browsers: BrowserManager,
    pub layout: LayoutState,
    pub stats: RuntimeStats,
    next_window_slot: usize,
    last_window_count: usize,
    active_toplevel_surface: Option<WlSurface>,
    pointer_hover_toplevel_surface: Option<WlSurface>,
    pending_pointer_focus_surface: Option<WlSurface>,
    window_layout_leaves: Vec<(WlSurface, LayoutLeafId)>,
    fullscreen_toplevels: Vec<(WlSurface, Option<wl_output::WlOutput>)>,
    next_pane_id: u64,
    window_pane_ids: Vec<(WlSurface, u64)>,
    pub(crate) ctrl_pressed: bool,
    pub(crate) alt_pressed: bool,
    pub(crate) ctrl_pressed_key: Option<smithay::backend::input::Keycode>,
    pub(crate) alt_pressed_key: Option<smithay::backend::input::Keycode>,
    pub(crate) ctrl_forwarded_to_client: bool,
    pub(crate) alt_forwarded_to_client: bool,
    pub(crate) suppress_mode_toggle_key_release: bool,
    pub input_mode: InputMode,
    managed_fullscreen: bool,
    active_surface_session: SurfaceSessionId,
    last_interacted_surface_session: SurfaceSessionId,
    surface_session_interaction_order: Vec<SurfaceSessionId>,
    next_surface_session: u64,
    surface_sessions: Vec<SurfaceSessionId>,
    surface_session_names: Vec<(SurfaceSessionId, String)>,
    window_surface_sessions: Vec<(WlSurface, SurfaceSessionId)>,
    presentation_member_surfaces: Vec<WlSurface>,
    extension_registry: ExtensionRegistry,
    session_dbus_buses: Vec<SessionDbusBus>,
    pub(crate) presentation_redraw_deferred: bool,
    pub(crate) presentation_skip_post_submit_frame_callbacks: bool,
    pub(crate) presenter: Option<Rc<PresenterHandle>>,
    pub(crate) wasm_key_handler: Option<WasmKeyHandler>,
    pub(crate) wasm_map_hook: Option<WasmMapHook>,
    pub(crate) test_harness: Option<TestHarnessRuntime>,
    pub(crate) test_harness_completion: Option<TestHarnessCompletion>,
    pub(crate) nested_inner_dbus_destination: Option<String>,
    pub(crate) nested_inner_dbus_address: Option<String>,
    pub(crate) manifold_session: Option<String>,
    pub(crate) manifold_dbus_destination: Option<String>,
    pub(crate) manifold_dbus_address: Option<String>,
}

impl Manifold {
    pub fn new(event_loop: &mut EventLoop<'static, Self>, display: Display<Self>) -> Self {
        let start_time = Instant::now();
        let display_handle = display.handle();
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        #[cfg(feature = "shm-fallback")]
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "manifold");
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("keyboard capability");
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop);
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
            space: Space::default(),
            compositor_state,
            xdg_shell_state,
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
            attach_requested: false,
            attach_schedule_pending: false,
            detach_requested: false,
            browsers: BrowserManager::default(),
            layout: LayoutState::default(),
            stats: RuntimeStats::default(),
            next_window_slot: 0,
            last_window_count: 0,
            active_toplevel_surface: None,
            pointer_hover_toplevel_surface: None,
            pending_pointer_focus_surface: None,
            window_layout_leaves: Vec::new(),
            fullscreen_toplevels: Vec::new(),
            next_pane_id: 1,
            window_pane_ids: Vec::new(),
            ctrl_pressed: false,
            alt_pressed: false,
            ctrl_pressed_key: None,
            alt_pressed_key: None,
            ctrl_forwarded_to_client: false,
            alt_forwarded_to_client: false,
            suppress_mode_toggle_key_release: false,
            input_mode: InputMode::Passthrough,
            managed_fullscreen: false,
            active_surface_session: SurfaceSessionId(1),
            last_interacted_surface_session: SurfaceSessionId(1),
            surface_session_interaction_order: vec![SurfaceSessionId(1)],
            next_surface_session: 2,
            surface_sessions: vec![SurfaceSessionId(1)],
            surface_session_names: vec![(SurfaceSessionId(1), "session 1".to_string())],
            window_surface_sessions: Vec::new(),
            presentation_member_surfaces: Vec::new(),
            extension_registry: ExtensionRegistry::default(),
            session_dbus_buses: Vec::new(),
            presentation_redraw_deferred: false,
            presentation_skip_post_submit_frame_callbacks: false,
            presenter: None,
            wasm_key_handler: None,
            wasm_map_hook: None,
            test_harness: None,
            test_harness_completion: None,
            nested_inner_dbus_destination: None,
            nested_inner_dbus_address: None,
            manifold_session: None,
            manifold_dbus_destination: None,
            manifold_dbus_address: None,
        }
    }

    fn init_wayland_listener(
        display: Display<Manifold>,
        event_loop: &mut EventLoop<'static, Self>,
    ) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto().expect("wayland socket");
        let socket_name = listening_socket.socket_name().to_os_string();
        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
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
                |_, _, state| {
                    state.refresh_window_state_from_lifecycle_timer();
                    TimeoutAction::ToDuration(WINDOW_LIFECYCLE_REFRESH_INTERVAL)
                },
            )
            .expect("insert window lifecycle timer");
    }

    pub fn spawn_hosted_app(&mut self, command: &OsString) -> Result<()> {
        let mut child = Command::new(command);
        child.env("WAYLAND_DISPLAY", &self.socket_name);
        self.apply_control_environment(&mut child);
        child.spawn().with_context(|| {
            format!(
                "spawn hosted Wayland app {:?} with WAYLAND_DISPLAY={}",
                command,
                self.socket_name.to_string_lossy()
            )
        })?;
        info!(
            command = ?command,
            wayland_display = %self.socket_name.to_string_lossy(),
            "spawned hosted Wayland app"
        );
        Ok(())
    }

    pub fn spawn_hosted_app_argv(&mut self, argv: &[String]) -> Result<()> {
        let Some((program, args)) = argv.split_first() else {
            anyhow::bail!("spawn requires a command");
        };

        let mut child = Command::new(program);
        child.args(args).env("WAYLAND_DISPLAY", &self.socket_name);
        self.apply_control_environment(&mut child);
        child.spawn().with_context(|| {
            format!(
                "spawn hosted Wayland app {:?} with WAYLAND_DISPLAY={}",
                argv,
                self.socket_name.to_string_lossy()
            )
        })?;
        info!(
            argv = ?argv,
            wayland_display = %self.socket_name.to_string_lossy(),
            "spawned hosted Wayland app"
        );
        Ok(())
    }

    pub fn spawn_browser_pane(
        &mut self,
        options: BrowserLaunchOptions,
    ) -> Result<&BrowserInstanceSummary> {
        self.browsers.spawn(options, &self.socket_name)
    }

    fn apply_control_environment(&mut self, child: &mut Command) {
        if let Some(session) = self.manifold_session.as_deref() {
            child.env(MANIFOLD_SESSION_ENV, session);
        }
        if let Some(destination) = self.manifold_dbus_destination.as_deref() {
            child.env(MANIFOLDD_DBUS_DESTINATION_ENV, destination);
        }
        if let Some(address) = self.manifold_dbus_address.clone() {
            child.env(MANIFOLDD_DBUS_ADDRESS_ENV, &address);
            let session = self.active_surface_session;
            match self.ensure_session_dbus_bus(session) {
                Ok(session_bus_address) => {
                    child.env("DBUS_SESSION_BUS_ADDRESS", session_bus_address);
                    child.env("MANIFOLD_SURFACE_SESSION_ID", session.raw().to_string());
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

    fn ensure_session_dbus_bus(&mut self, session: SurfaceSessionId) -> Result<String> {
        if let Some(bus) = self
            .session_dbus_buses
            .iter()
            .find(|bus| bus.session == session)
        {
            return Ok(bus.address.clone());
        }

        let dir = std::env::temp_dir().join(format!(
            "manifold-session-dbus-{}-{}",
            std::process::id(),
            session.raw()
        ));
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let config = dir.join("session.conf");
        let bus_info = dir.join("bus.info");
        let config_text = format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow user="*"/>
    <allow own="*"/>
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
  </policy>
</busconfig>
"#,
            dir.display()
        );
        fs::write(&config, config_text).with_context(|| format!("write {}", config.display()))?;

        let output = fs::File::create(&bus_info)
            .with_context(|| format!("create {}", bus_info.display()))?;
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .arg("--fork")
            .arg("--print-address=1")
            .arg("--print-pid=1")
            .stdout(Stdio::from(output))
            .spawn()
            .context("spawn session DBus daemon")?;

        let (address, daemon_pid) = wait_for_dbus_info(&bus_info)?;
        let _ = child.wait();
        self.session_dbus_buses.push(SessionDbusBus {
            session,
            address: address.clone(),
            daemon_pid,
            dir,
        });
        info!(
            session = session.raw(),
            dbus_address = %address,
            daemon_pid,
            "started session-local DBus bus"
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
    }

    pub fn schedule_attach(&mut self) {
        let handle = self.loop_handle.clone();
        let result = self
            .loop_handle
            .insert_source(Timer::immediate(), move |_, _, state| {
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
                let local_pos =
                    Point::from((pos.x - f64::from(location.x), pos.y - f64::from(location.y)));
                window
                    .surface_under(local_pos, WindowSurfaceType::ALL)
                    .map(|(surface, point)| (surface, point.to_f64()))
            })
    }

    pub fn window_under_layout_position(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(Window, Point<i32, Logical>)> {
        if let Some(active_window) = self.active_window() {
            if let Some(location) = self.window_hit_location(&active_window, pos) {
                return Some((active_window, location));
            }
        }

        self.space
            .elements()
            .filter(|window| self.window_in_active_surface_session(window))
            .filter_map(|window| {
                self.window_hit_location(window, pos)
                    .map(|location| (window.clone(), location))
            })
            .last()
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
        let local_pos = Point::from((pos.x - f64::from(rect.x), pos.y - f64::from(rect.y)));
        window
            .surface_under(local_pos, WindowSurfaceType::ALL)
            .is_some()
            .then(|| Point::from((rect.x, rect.y)))
    }

    pub fn first_window_center(&self) -> Option<Point<f64, Logical>> {
        self.space.elements().find_map(|window| {
            let bbox = self.space.element_bbox(window)?.to_f64();
            Some(Point::from((
                bbox.loc.x + bbox.size.w / 2.0,
                bbox.loc.y + bbox.size.h / 2.0,
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

        if self.test_harness_completion.is_none() {
            if let Some(completion) = harness.try_take_completion() {
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
        let client_leaf_active = self.layout.active_kind() == Some(LayoutLeafKind::Client);
        self.space.elements().for_each(|window| {
            let Some(toplevel) = window.toplevel() else {
                return;
            };
            let is_fullscreen = self
                .toplevel_fullscreen_output(toplevel.wl_surface())
                .is_some();
            let fullscreen_output = self
                .toplevel_fullscreen_output(toplevel.wl_surface())
                .cloned()
                .flatten();
            let client_size = self.window_layout_rect(window).map_or_else(
                || self.presentation_size(),
                |rect| (rect.w.max(1), rect.h.max(1)),
            );
            let is_active = client_leaf_active && active_surface == Some(toplevel.wl_surface());
            toplevel.with_pending_state(|state| {
                state.size = Some(client_size.into());
                state.bounds = Some(client_size.into());
                if is_fullscreen || self.managed_fullscreen {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    state.states.unset(xdg_toplevel::State::Maximized);
                } else {
                    state.states.set(xdg_toplevel::State::Maximized);
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                }
                if is_active {
                    state.states.set(xdg_toplevel::State::Activated);
                } else {
                    state.states.unset(xdg_toplevel::State::Activated);
                }
                state.fullscreen_output = fullscreen_output;
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
        self.window_surface_sessions
            .push((surface.clone(), self.active_surface_session));
        let pane = self.register_toplevel_pane(surface);
        let leaf = if self.layout.active_kind() == Some(LayoutLeafKind::Placeholder) {
            self.layout.occupy_active_leaf()
        } else {
            Some(self.layout.active_leaf())
        };
        if let Some(leaf) = leaf {
            self.assign_pane_to_leaf(pane, leaf);
        }
        self.apply_map_hook_for_pending_pane(pane)
    }

    fn register_toplevel_pane(&mut self, surface: &WlSurface) -> u64 {
        if let Some(pane) = self.pane_for_toplevel_surface(surface) {
            return pane;
        }
        let pane = self.next_pane_id;
        self.next_pane_id += 1;
        self.window_pane_ids.push((surface.clone(), pane));
        pane
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
            .filter_map(|(surface, _)| self.pane_for_toplevel_surface(surface))
            .fold(Vec::new(), |mut panes, pane| {
                if !panes.contains(&pane) {
                    panes.push(pane);
                }
                panes
            })
    }

    fn leaf_for_pane(&self, target_pane: u64) -> Option<LayoutLeafId> {
        let surface = self.surface_for_pane(target_pane)?;
        self.leaf_for_toplevel_surface(&surface)
    }

    fn leaf_for_toplevel_surface(&self, surface: &WlSurface) -> Option<LayoutLeafId> {
        self.window_layout_leaves
            .iter()
            .rev()
            .find_map(|(assigned_surface, leaf)| (assigned_surface == surface).then_some(*leaf))
    }

    fn assign_pane_to_leaf(&mut self, pane: u64, leaf: LayoutLeafId) -> bool {
        let Some(surface) = self.surface_for_pane(pane) else {
            return false;
        };
        self.window_layout_leaves
            .retain(|(assigned_surface, _)| assigned_surface != &surface);
        self.window_layout_leaves.push((surface, leaf));
        true
    }

    fn apply_map_hook_for_pending_pane(&mut self, pending_pane: u64) -> Option<WlSurface> {
        let visible_panes = self.active_surface_session_panes();
        let Some(handler) = self.wasm_map_hook.as_mut() else {
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
                return self.surface_for_pane(pending_pane);
            }
        };
        match outcome {
            MapHookOutcome::Abstain => self.surface_for_pane(pending_pane),
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
                None
            }
            MapHookResponse::Patch { patches } => self.apply_map_hook_patches(patches),
            MapHookResponse::ReplaceVisibleTree { root, focus } => {
                self.apply_map_hook_visible_tree(root, focus)
            }
        }
    }

    fn apply_map_hook_patches(&mut self, patches: Vec<MapLayoutPatch>) -> Option<WlSurface> {
        let mut focus_surface = None;
        for patch in patches {
            match patch {
                MapLayoutPatch::PlacePane { pane, after, focus } => {
                    let Some(target_leaf) = after
                        .and_then(|anchor| self.leaf_for_pane(anchor))
                        .or_else(|| Some(self.layout.active_leaf()))
                    else {
                        continue;
                    };
                    if self.assign_pane_to_leaf(pane, target_leaf) {
                        if focus {
                            self.layout.set_active_leaf(target_leaf);
                            focus_surface = self.surface_for_pane(pane);
                        }
                    }
                }
            }
        }
        self.reconfigure_focus_stack();
        focus_surface
    }

    fn apply_map_hook_visible_tree(
        &mut self,
        root: crate::layout::VisibleLayoutNode,
        focus: Option<u64>,
    ) -> Option<WlSurface> {
        let expected = self.active_surface_session_panes();
        let mut next_layout = self.layout.clone();
        let applied = match next_layout.replace_visible_tree(&root, focus) {
            Ok(applied) => applied,
            Err(err) => {
                warn!(
                    ?err,
                    "WASM map hook returned invalid visible tree; using native placement"
                );
                return None;
            }
        };

        let mut assigned = applied
            .assignments
            .iter()
            .map(|assignment| assignment.pane)
            .collect::<Vec<_>>();
        assigned.sort_unstable();
        let mut expected_sorted = expected;
        expected_sorted.sort_unstable();
        if assigned != expected_sorted {
            warn!(
                assigned = ?assigned,
                expected = ?expected_sorted,
                "WASM map hook visible tree must reference exactly the active session panes"
            );
            return None;
        }

        let mut next_window_layout_leaves = self
            .window_layout_leaves
            .iter()
            .filter(|(surface, _)| {
                self.session_for_toplevel_surface(surface)
                    .unwrap_or(SurfaceSessionId(1))
                    != self.active_surface_session
            })
            .cloned()
            .collect::<Vec<_>>();
        for assignment in applied.assignments {
            let Some(surface) = self.surface_for_pane(assignment.pane) else {
                warn!(
                    pane = assignment.pane,
                    "WASM map hook referenced pane without a surface"
                );
                return None;
            };
            next_window_layout_leaves.push((surface, assignment.leaf));
        }

        let focus_surface = applied
            .focused_pane
            .and_then(|pane| self.surface_for_pane(pane));
        self.layout = next_layout;
        self.window_layout_leaves = next_window_layout_leaves;
        self.reconfigure_focus_stack();
        focus_surface
    }

    pub fn active_surface_session(&self) -> SurfaceSessionId {
        self.active_surface_session
    }

    pub fn restore_last_interacted_surface_session(&mut self) -> bool {
        let Some(session) = self.most_recent_live_surface_session() else {
            return false;
        };
        if self.active_surface_session == session {
            return false;
        }
        self.active_surface_session = session;
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
                        let Some(surface) = Manifold::window_toplevel_surface(window) else {
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
        if self.ctrl_forwarded_to_client {
            if let Some(key_code) = self.ctrl_pressed_key {
                self.seat.get_keyboard().expect("keyboard").input::<(), _>(
                    self,
                    key_code,
                    smithay::backend::input::KeyState::Released,
                    SERIAL_COUNTER.next_serial(),
                    0,
                    |_, _, _| smithay::input::keyboard::FilterResult::Forward,
                );
            }
        }
        if self.alt_forwarded_to_client {
            if let Some(key_code) = self.alt_pressed_key {
                self.seat.get_keyboard().expect("keyboard").input::<(), _>(
                    self,
                    key_code,
                    smithay::backend::input::KeyState::Released,
                    SERIAL_COUNTER.next_serial(),
                    0,
                    |_, _, _| smithay::input::keyboard::FilterResult::Forward,
                );
            }
        }

        self.ctrl_pressed = false;
        self.alt_pressed = false;
        self.ctrl_pressed_key = None;
        self.alt_pressed_key = None;
        self.ctrl_forwarded_to_client = false;
        self.alt_forwarded_to_client = false;
        self.suppress_mode_toggle_key_release = false;
        self.pending_pointer_focus_surface = None;
        self.pointer_hover_toplevel_surface = None;
    }

    pub fn surface_session_count(&self) -> usize {
        self.surface_sessions.len()
    }

    pub fn surface_session_summaries(&self) -> Vec<SurfaceSessionSummary> {
        self.surface_sessions
            .iter()
            .copied()
            .map(|session| SurfaceSessionSummary {
                id: session.raw(),
                name: self.surface_session_name(session).to_string(),
                active: session == self.active_surface_session,
                windows: self.surface_session_window_count(session),
            })
            .collect()
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

    pub fn close_surface_session(&mut self, session: SurfaceSessionId) -> bool {
        if self.surface_sessions.len() <= 1 || !self.surface_sessions.contains(&session) {
            return false;
        }

        let surfaces = self
            .window_surface_sessions
            .iter()
            .filter_map(|(surface, assigned_session)| {
                (*assigned_session == session).then(|| surface.clone())
            })
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
        self.surface_session_interaction_order
            .retain(|existing| *existing != session);
        self.window_surface_sessions
            .retain(|(_, assigned_session)| *assigned_session != session);
        self.window_layout_leaves
            .retain(|(surface, _)| !surfaces.iter().any(|closed| closed == surface));
        if self.active_surface_session == session {
            self.active_surface_session = self
                .surface_session_interaction_order
                .iter()
                .copied()
                .find(|candidate| self.surface_sessions.contains(candidate))
                .or_else(|| self.surface_sessions.first().copied())
                .unwrap_or(SurfaceSessionId(1));
        }
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
                let Some(surface) = Manifold::window_toplevel_surface(window) else {
                    return false;
                };
                let assigned_session = self
                    .session_for_toplevel_surface(&surface)
                    .unwrap_or(SurfaceSessionId(1));
                let presentation_member = self
                    .presentation_member_surfaces
                    .iter()
                    .any(|member| member == &surface);
                assigned_session == session
                    || (session == self.active_surface_session && presentation_member)
            })
            .count()
    }

    pub fn switch_to_surface_session(&mut self, session: SurfaceSessionId) -> bool {
        if !self.surface_sessions.contains(&session) || self.active_surface_session == session {
            return false;
        }
        self.active_surface_session = session;
        self.record_surface_session_interaction(session);
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
            self.reconfigure_focus_stack();
            return true;
        };
        if self.surface_sessions.len() <= 1 {
            return false;
        }
        self.active_surface_session =
            self.surface_sessions[(active_index + 1) % self.surface_sessions.len()];
        self.record_surface_session_interaction(self.active_surface_session);
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
        let session = SurfaceSessionId(self.next_surface_session);
        self.next_surface_session += 1;
        self.surface_sessions.push(session);
        self.surface_session_names.push((
            session,
            name.filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| format!("session {}", session.raw())),
        ));
        self.active_surface_session = session;
        self.record_surface_session_interaction(session);
        self.reconfigure_focus_stack();
        session
    }

    fn session_for_toplevel_surface(&self, surface: &WlSurface) -> Option<SurfaceSessionId> {
        self.window_surface_sessions
            .iter()
            .rev()
            .find_map(|(assigned_surface, session)| {
                (assigned_surface == surface).then_some(*session)
            })
    }

    pub fn window_in_active_surface_session(&self, window: &Window) -> bool {
        let Some(surface) = Manifold::window_toplevel_surface(window) else {
            return false;
        };
        if self
            .presentation_member_surfaces
            .iter()
            .any(|member| member == &surface)
        {
            return true;
        }
        self.session_for_toplevel_surface(&surface)
            .unwrap_or(SurfaceSessionId(1))
            == self.active_surface_session
    }

    pub fn window_layout_rect(&self, window: &Window) -> Option<LayoutRect> {
        if !self.window_in_active_surface_session(window) {
            return None;
        }
        let surface = Manifold::window_toplevel_surface(window)?;
        if self.toplevel_fullscreen_output(&surface).is_some() {
            return Some(fullscreen_layout_rect(self.presentation_layout_size()));
        }
        let leaf = self
            .window_layout_leaves
            .iter()
            .rev()
            .find_map(|(assigned_surface, leaf)| (assigned_surface == &surface).then_some(*leaf))?;
        self.layout.leaf_rect(leaf, self.presentation_layout_size())
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

    pub fn requested_fullscreen_count(&self) -> usize {
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
        self.window_layout_leaves
            .iter()
            .rev()
            .find_map(|(surface, leaf)| {
                (*leaf == target_leaf
                    && self
                        .session_for_toplevel_surface(surface)
                        .unwrap_or(SurfaceSessionId(1))
                        == self.active_surface_session)
                    .then(|| surface.clone())
            })
    }

    fn focus_active_layout_leaf(&mut self) {
        let active_surface = if self.layout.active_kind() == Some(LayoutLeafKind::Client) {
            self.surface_for_layout_leaf(self.layout.active_leaf())
        } else {
            None
        };
        self.focus_toplevel_surface(active_surface);
        self.reconfigure_focus_stack();
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
            .and_then(|surface| self.leaf_for_toplevel_surface(surface))
        {
            self.layout.set_active_leaf(leaf);
        }
        self.active_toplevel_surface = active_surface.clone();
        self.activate_window_exclusively(active_surface.as_ref());
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard_focus = if self.layout.active_kind() == Some(LayoutLeafKind::Client) {
            active_surface
        } else {
            None
        };
        self.seat
            .get_keyboard()
            .expect("keyboard")
            .set_focus(self, keyboard_focus, serial);
    }

    pub fn active_window(&self) -> Option<Window> {
        self.active_toplevel_surface.as_ref().and_then(|active| {
            self.space
                .elements()
                .find(|window| {
                    self.window_in_active_surface_session(window)
                        && Manifold::window_toplevel_surface(window).as_ref() == Some(active)
                })
                .cloned()
        })
    }

    pub fn focus_window(&mut self, window: &Window) {
        self.space.raise_element(window, true);
        self.focus_toplevel_surface(Manifold::window_toplevel_surface(window));
    }

    pub fn focus_app_id(&mut self, target_app_id: &str) -> bool {
        let window = self
            .space
            .elements()
            .filter(|window| self.window_in_active_surface_session(window))
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

    pub fn request_presentation_membership(
        &mut self,
        window_id: &str,
        membership: ExtensionMetadata,
        sender: String,
    ) -> Result<String> {
        ensure_metadata(&membership)?;
        let window = self.window_record(window_id)?;
        let mut detail = membership.clone();
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
        self.focus_toplevel_surface(Some(surface));
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
                self.window_in_active_surface_session(window)
                    && Manifold::window_toplevel_surface(window).as_ref() == Some(surface)
            })
            .cloned()
    }

    fn window_for_any_toplevel_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|window| Manifold::window_toplevel_surface(window).as_ref() == Some(surface))
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
        self.reconfigure_focus_stack();
        true
    }

    fn find_surface_for_window_identity(&self, identity: &ExtensionMetadata) -> Option<WlSurface> {
        self.space.elements().find_map(|window| {
            let surface = Manifold::window_toplevel_surface(window)?;
            toplevel_matches_identity(self, &surface, identity).then_some(surface)
        })
    }

    pub fn set_pending_pointer_focus_surface(&mut self, surface: Option<WlSurface>) {
        self.pending_pointer_focus_surface = surface;
    }

    pub fn take_pending_pointer_focus_surface(&mut self) -> Option<WlSurface> {
        self.pending_pointer_focus_surface.take()
    }

    pub fn set_pointer_hover_toplevel_surface(&mut self, surface: Option<WlSurface>) {
        self.pointer_hover_toplevel_surface = surface;
    }

    pub fn pointer_hover_toplevel_surface(&self) -> Option<WlSurface> {
        self.pointer_hover_toplevel_surface.clone()
    }

    pub fn cycle_focus(&mut self, direction: FocusDirection) -> bool {
        let windows = self
            .space
            .elements()
            .filter(|window| self.window_in_active_surface_session(window))
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
                    Manifold::window_toplevel_surface(window).as_ref() == Some(active)
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

    pub fn focused_window_index(&self) -> Option<usize> {
        if self.layout.active_kind() != Some(LayoutLeafKind::Client) {
            return None;
        }
        let active = self.active_toplevel_surface.as_ref()?;
        self.space
            .elements()
            .filter(|window| self.window_in_active_surface_session(window))
            .position(|window| Manifold::window_toplevel_surface(window).as_ref() == Some(active))
    }

    pub fn focused_window_count(&self) -> usize {
        usize::from(
            self.layout.active_kind() == Some(LayoutLeafKind::Client)
                && self.active_window().is_some(),
        )
    }

    pub fn focused_surface_id(&self) -> Option<String> {
        if self.layout.active_kind() != Some(LayoutLeafKind::Client) {
            return None;
        }
        self.active_toplevel_surface.as_ref().map(|surface| {
            crate::presentation::sanitize_status_value(&format!("{:?}", surface.id()))
        })
    }

    pub fn reconfigure_focus_stack(&mut self) {
        let windows = self
            .space
            .elements()
            .filter(|window| self.window_in_active_surface_session(window))
            .cloned()
            .collect::<Vec<_>>();
        for window in &windows {
            let location = self.window_layout_rect(window).map_or_else(
                || focus_stack_location(0),
                |rect| Point::from((rect.x, rect.y)),
            );
            self.space.map_element(window.clone(), location, false);
        }
        if self.layout.active_kind() == Some(LayoutLeafKind::Placeholder) {
            self.focus_toplevel_surface(None);
            return;
        }
        if self.active_window().is_none() {
            let active_leaf_surface = self.surface_for_layout_leaf(self.layout.active_leaf());
            if active_leaf_surface.is_some() {
                self.focus_toplevel_surface(active_leaf_surface);
            }
        }
        if let Some(active_window) = self.active_window() {
            self.space.raise_element(&active_window, true);
            self.activate_window_exclusively(self.active_toplevel_surface.clone().as_ref());
        } else if let Some(window) = windows.last() {
            self.focus_window(window);
        } else {
            self.focus_toplevel_surface(None);
        }
    }

    pub fn toggle_input_mode(&mut self) -> InputMode {
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

    pub fn managed_fullscreen(&self) -> bool {
        self.managed_fullscreen
    }

    pub fn apply_layout_action(&mut self, action: LayoutAction) -> bool {
        match action {
            LayoutAction::FocusNext => {
                if self.layout.focus_next() {
                    self.focus_active_layout_leaf();
                    true
                } else {
                    self.cycle_focus(FocusDirection::Next)
                }
            }
            LayoutAction::FocusPrevious => {
                if self.layout.focus_previous() {
                    self.focus_active_layout_leaf();
                    true
                } else {
                    self.cycle_focus(FocusDirection::Previous)
                }
            }
            LayoutAction::CloseActivePane => self.close_active_window(),
            LayoutAction::ToggleManagedFullscreen => {
                self.toggle_managed_fullscreen();
                true
            }
            LayoutAction::SplitActiveRight => {
                let changed = self.layout.split_active(LayoutAxis::Horizontal);
                if changed {
                    self.reconfigure_focus_stack();
                }
                changed
            }
            LayoutAction::SplitActiveDown => {
                let changed = self.layout.split_active(LayoutAxis::Vertical);
                if changed {
                    self.reconfigure_focus_stack();
                }
                changed
            }
            LayoutAction::JoinActivePane
            | LayoutAction::MoveActiveToLeftDock
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
                self.layout.left_dock_visible = !self.layout.left_dock_visible;
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
        self.window_layout_leaves
            .retain(|(surface, _)| live_toplevel_surfaces.iter().any(|live| live == surface));
        self.fullscreen_toplevels
            .retain(|(surface, _)| live_toplevel_surfaces.iter().any(|live| live == surface));
        self.window_surface_sessions
            .retain(|(surface, _)| live_toplevel_surfaces.iter().any(|live| live == surface));
        self.presentation_member_surfaces
            .retain(|surface| live_toplevel_surfaces.iter().any(|live| live == surface));
        self.window_pane_ids
            .retain(|(surface, _)| live_toplevel_surfaces.iter().any(|live| live == surface));
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
                live_toplevel_surfaces.iter().any(|live| live == surface)
                    && self
                        .session_for_toplevel_surface(surface)
                        .unwrap_or(SurfaceSessionId(1))
                        == self.active_surface_session
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
                    && self
                        .session_for_toplevel_surface(surface)
                        .unwrap_or(SurfaceSessionId(1))
                        == self.active_surface_session
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
        LayoutSize { w, h }
    }

    pub fn active_layout_rect(&self) -> Option<LayoutRect> {
        self.layout.active_rect(self.presentation_layout_size())
    }
}

fn fullscreen_layout_rect(size: LayoutSize) -> LayoutRect {
    LayoutRect {
        x: 0,
        y: 0,
        w: size.w.max(1),
        h: size.h.max(1),
    }
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

fn stable_registry_id<'a, S>(
    prefix: &str,
    parts: impl IntoIterator<Item = &'a S>,
    metadata: &ExtensionMetadata,
) -> String
where
    S: AsRef<str> + 'a + ?Sized,
{
    let mut hash = 0xcbf29ce484222325_u64;
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
        *hash = hash.wrapping_mul(0x100000001b3);
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

fn wait_for_dbus_info(path: &std::path::Path) -> Result<(String, u32)> {
    let start = Instant::now();
    loop {
        if let Ok(contents) = fs::read_to_string(path)
            && let Some(info) = parse_dbus_info(&contents)
        {
            return Ok(info);
        }
        if start.elapsed() > Duration::from_secs(5) {
            anyhow::bail!(
                "timed out waiting for DBus daemon info at {}",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn parse_dbus_info(contents: &str) -> Option<(String, u32)> {
    let mut lines = contents.lines();
    let address = lines.next()?.trim();
    if address.is_empty() {
        return None;
    }
    let daemon_pid = lines.next()?.trim().parse().ok()?;
    Some((address.to_string(), daemon_pid))
}

#[derive(Default)]
struct WindowRefreshOutcome {
    focus_cleared: bool,
    focus_repaired: bool,
}

#[cfg(test)]
mod tests {
    use super::{focus_stack_location, fullscreen_layout_rect};
    use crate::layout::LayoutSize;

    #[test]
    fn focus_stack_places_every_window_at_origin() {
        let first = focus_stack_location(0);
        let later = focus_stack_location(99);

        assert_eq!((first.x, first.y), (0, 0));
        assert_eq!((later.x, later.y), (0, 0));
    }

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

    pub fn record_attach_failed(&mut self) {
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

    fn clear_pending_visible_latency_samples(&mut self) {
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

    pub fn record_input_frame_deferred(&mut self) {
        self.input_frame_deferrals += 1;
    }

    pub fn record_input_deferred_frame(&mut self) {
        self.input_deferred_frames += 1;
    }

    pub fn record_client_frame_deferred(&mut self) {
        self.client_frame_deferrals += 1;
    }

    pub fn record_client_deferred_frame(&mut self) {
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

            if let Some(input_at) = self.pending_input_at {
                if self.pending_input_commit_at.is_none() {
                    self.pending_input_to_commit_micros =
                        Some(now.duration_since(input_at).as_micros());
                    self.pending_input_commit_at = Some(now);
                }
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
