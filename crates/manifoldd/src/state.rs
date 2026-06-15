use std::{
    cell::RefCell,
    ffi::OsString,
    process::Command,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
#[cfg(feature = "shm-fallback")]
use smithay::wayland::shm::ShmState;
use smithay::{
    backend::{renderer::gles::GlesRenderer, winit::WinitGraphicsBackend},
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
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufFeedback, DmabufGlobal, DmabufState},
        drm_syncobj::DrmSyncobjState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::XdgShellState,
        socket::ListeningSocketSource,
    },
};
use tracing::{info, warn};

use crate::browser::{BrowserInstanceSummary, BrowserLaunchOptions, BrowserManager};

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
    pub detach_requested: bool,
    pub browsers: BrowserManager,
    pub stats: RuntimeStats,
    next_window_slot: usize,
    last_window_count: usize,
    active_toplevel_surface: Option<WlSurface>,
    pub(crate) ctrl_pressed: bool,
    pub(crate) ctrl_forwarded_to_client: bool,
    pub(crate) suppress_mode_toggle_key_release: bool,
    pub input_mode: InputMode,
    managed_fullscreen: bool,
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
            detach_requested: false,
            browsers: BrowserManager::default(),
            stats: RuntimeStats::default(),
            next_window_slot: 0,
            last_window_count: 0,
            active_toplevel_surface: None,
            ctrl_pressed: false,
            ctrl_forwarded_to_client: false,
            suppress_mode_toggle_key_release: false,
            input_mode: InputMode::Passthrough,
            managed_fullscreen: false,
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

    pub fn spawn_hosted_app(&self, command: &OsString) -> Result<()> {
        let mut child = Command::new(command);
        child.env("WAYLAND_DISPLAY", &self.socket_name);
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

    pub fn spawn_hosted_app_argv(&self, argv: &[String]) -> Result<()> {
        let Some((program, args)) = argv.split_first() else {
            anyhow::bail!("spawn requires a command");
        };

        let mut child = Command::new(program);
        child.args(args).env("WAYLAND_DISPLAY", &self.socket_name);
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

    pub fn record_presentation_attach_failure(&mut self, error: &str) {
        self.stats.record_attach_failed();
        self.presentation_attach_failures += 1;
        self.presentation_last_error = Some(crate::presentation::sanitize_status_value(error));
        self.presentation = PresentationStatus::Detached;
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
        self.active_window().and_then(|window| {
            window
                .surface_under(pos, WindowSurfaceType::ALL)
                .map(|(surface, point)| (surface, point.to_f64()))
        })
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

    pub fn activate_window_exclusively(&mut self, active_surface: Option<&WlSurface>) {
        self.space.elements().for_each(|window| {
            let Some(toplevel) = window.toplevel() else {
                return;
            };
            let is_active = active_surface == Some(toplevel.wl_surface());
            let size = self.presentation_size();
            toplevel.with_pending_state(|state| {
                state.size = Some(size.into());
                state.bounds = Some(size.into());
                if self.managed_fullscreen {
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
                state.fullscreen_output = None;
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
        self.last_window_count = self.space.elements().count();
        self.reconfigure_focus_stack();
    }

    pub fn focus_toplevel_surface(&mut self, active_surface: Option<WlSurface>) {
        self.active_toplevel_surface = active_surface.clone();
        self.activate_window_exclusively(active_surface.as_ref());
        let serial = SERIAL_COUNTER.next_serial();
        self.seat
            .get_keyboard()
            .expect("keyboard")
            .set_focus(self, active_surface, serial);
    }

    pub fn active_window(&self) -> Option<Window> {
        self.active_toplevel_surface.as_ref().and_then(|active| {
            self.space
                .elements()
                .find(|window| Manifold::window_toplevel_surface(window).as_ref() == Some(active))
                .cloned()
        })
    }

    pub fn focus_window(&mut self, window: &Window) {
        self.space.raise_element(window, true);
        self.focus_toplevel_surface(Manifold::window_toplevel_surface(window));
    }

    pub fn cycle_focus(&mut self, direction: FocusDirection) -> bool {
        let windows = self.space.elements().cloned().collect::<Vec<_>>();
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
        let active = self.active_toplevel_surface.as_ref()?;
        self.space
            .elements()
            .position(|window| Manifold::window_toplevel_surface(window).as_ref() == Some(active))
    }

    pub fn focused_window_count(&self) -> usize {
        usize::from(self.active_window().is_some())
    }

    pub fn focused_surface_id(&self) -> Option<String> {
        self.active_toplevel_surface.as_ref().map(|surface| {
            crate::presentation::sanitize_status_value(&format!("{:?}", surface.id()))
        })
    }

    pub fn reconfigure_focus_stack(&mut self) {
        let windows = self.space.elements().cloned().collect::<Vec<_>>();
        for window in &windows {
            self.space
                .map_element(window.clone(), focus_stack_location(0), false);
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

    pub fn close_active_window(&mut self) -> bool {
        let Some(window) = self.active_window() else {
            return false;
        };
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        toplevel.send_close();
        true
    }

    pub fn spawn_wezterm_pane(&self) -> Result<()> {
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
        self.reconfigure_focus_stack();

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
        let window_count = live_toplevel_surfaces.len();

        let keyboard = self.seat.get_keyboard().expect("keyboard");
        if let Some(current_focus) = keyboard.current_focus() {
            let focus_is_live = live_toplevel_surfaces
                .iter()
                .any(|surface| surface == &current_focus);
            if !focus_is_live {
                if let Some(next_focus) = live_toplevel_surfaces.first().cloned() {
                    self.focus_toplevel_surface(Some(next_focus));
                    self.stats.focus_repairs += 1;
                    outcome.focus_repaired = true;
                } else {
                    self.focus_toplevel_surface(None);
                    self.stats.focus_clears += 1;
                    outcome.focus_cleared = true;
                }
            } else if self.last_window_count > 0 && window_count == 0 {
                self.focus_toplevel_surface(None);
                self.stats.focus_clears += 1;
                outcome.focus_cleared = true;
            }
        }
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
}

fn focus_stack_location(_slot: usize) -> Point<i32, Logical> {
    Point::from((0, 0))
}

#[derive(Default)]
struct WindowRefreshOutcome {
    focus_cleared: bool,
    focus_repaired: bool,
}

#[cfg(test)]
mod tests {
    use super::focus_stack_location;

    #[test]
    fn focus_stack_places_every_window_at_origin() {
        let first = focus_stack_location(0);
        let later = focus_stack_location(99);

        assert_eq!((first.x, first.y), (0, 0));
        assert_eq!((later.x, later.y), (0, 0));
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
    pub importer: Option<Rc<RefCell<WinitGraphicsBackend<GlesRenderer>>>>,
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
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
