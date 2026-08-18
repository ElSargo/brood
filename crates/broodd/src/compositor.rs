//! Smithay protocol wiring for the nested compositor.
//!
//! Process/runtime startup lives in `runtime`, and built-in nested-test process
//! orchestration lives in `nested_test`.

#[cfg(feature = "shm-fallback")]
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    backend::{
        allocator::{Buffer, dmabuf::Dmabuf},
        renderer::{ImportDma, buffer_dimensions, utils::on_commit_buffer_handler},
    },
    desktop::{PopupKind, Window},
    input::{Seat, SeatHandler, SeatState},
    reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    reexports::{
        calloop::Interest,
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer, wl_output, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface,
            with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf},
        drm_syncobj::{DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState},
        fractional_scale::FractionalScaleHandler,
        output::OutputHandler,
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
            ext_data_control::{
                DataControlHandler as ExtDataControlHandler,
                DataControlState as ExtDataControlState,
            },
            wlr_data_control::{
                DataControlHandler as WlrDataControlHandler,
                DataControlState as WlrDataControlState,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData, decoration::XdgDecorationHandler,
        },
    },
};
use tracing::warn;

use crate::{
    clipboard::{ClipboardSelection, send_clipboard_selection},
    state::{Brood, ClientState},
};

impl CompositorHandler for Brood {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    #[allow(
        clippy::expect_used,
        reason = "the Wayland display installs ClientState for every accepted client"
    )]
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client state")
            .compositor_state
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the Smithay cached-state guard must remain alive while its pending sync state is inspected"
    )]
    fn new_surface(&mut self, surface: &WlSurface) {
        self.register_surface(surface);
        add_pre_commit_hook::<Self, _>(surface, move |state, _dh, surface| {
            let mut acquire_point = None;
            let mut has_release_point = false;
            let (maybe_dmabuf, new_buffer_size) = with_states(surface, |surface_data| {
                {
                    let mut syncobj_guard =
                        surface_data.cached_state.get::<DrmSyncobjCachedState>();
                    let syncobj_state = syncobj_guard.pending();
                    acquire_point.clone_from(&syncobj_state.acquire_point);
                    has_release_point = syncobj_state.release_point.is_some();
                }
                let mut attributes = surface_data.cached_state.get::<SurfaceAttributes>();
                let attributes = attributes.pending();
                let transform = smithay::utils::Transform::from(attributes.buffer_transform);
                let new_buffer = attributes.buffer.as_ref().and_then(|assignment| {
                    if let BufferAssignment::NewBuffer(buffer) = assignment {
                        Some(buffer)
                    } else {
                        None
                    }
                });
                let maybe_dmabuf = new_buffer.and_then(|buffer| get_dmabuf(buffer).cloned().ok());
                let new_buffer_size = new_buffer
                    .and_then(buffer_dimensions)
                    .map(|size| transform.transform_size(size))
                    .map(|size| (size.w, size.h));
                (maybe_dmabuf, new_buffer_size)
            });

            state.record_managed_commit_observation(surface, new_buffer_size);

            if let Some(dmabuf) = maybe_dmabuf {
                if has_release_point {
                    state.dmabuf.stats.explicit_sync_release_points += 1;
                }

                match dmabuf_blocker_plan(true, acquire_point.is_some()) {
                    DmabufBlockerPlan::ExplicitSync => {
                        let Some(acquire_point) = acquire_point else {
                            state.record_dmabuf_blocker_failure(
                                "explicit_sync_plan_without_acquire_point",
                            );
                            return;
                        };
                        match acquire_point.generate_blocker() {
                            Ok((blocker, source)) => {
                                if let Some(client) = surface.client() {
                                    let result = state.loop_handle.insert_source(
                                        source,
                                        move |(), (), data| {
                                            let display_handle = data.display_handle.clone();
                                            data.client_compositor_state(&client)
                                                .blocker_cleared(data, &display_handle);
                                            Ok(())
                                        },
                                    );
                                    match result {
                                        Ok(_) => {
                                            state.dmabuf.stats.explicit_sync_blockers += 1;
                                            state.dmabuf.stats.last_blocker_error = None;
                                            add_blocker(surface, blocker);
                                        }
                                        Err(err) => {
                                            state.record_dmabuf_blocker_failure(&format!(
                                                "explicit_sync_insert_source:{err:?}"
                                            ));
                                        }
                                    }
                                } else {
                                    state.record_dmabuf_blocker_failure("explicit_sync_no_client");
                                }
                            }
                            Err(_already_ready) => {
                                state.dmabuf.stats.last_blocker_error = None;
                            }
                        }
                        return;
                    }
                    DmabufBlockerPlan::ImplicitDmabuf => {}
                    DmabufBlockerPlan::None => return,
                }

                match dmabuf.generate_blocker(Interest::READ) {
                    Ok((blocker, source)) => {
                        if let Some(client) = surface.client() {
                            let result =
                                state.loop_handle.insert_source(source, move |(), _, data| {
                                    let display_handle = data.display_handle.clone();
                                    data.client_compositor_state(&client)
                                        .blocker_cleared(data, &display_handle);
                                    Ok(())
                                });
                            match result {
                                Ok(_) => {
                                    state.dmabuf.stats.implicit_dmabuf_blockers += 1;
                                    state.dmabuf.stats.last_blocker_error = None;
                                    add_blocker(surface, blocker);
                                }
                                Err(err) => {
                                    state.record_dmabuf_blocker_failure(&format!(
                                        "implicit_dmabuf_insert_source:{err:?}"
                                    ));
                                }
                            }
                        } else {
                            state.record_dmabuf_blocker_failure("implicit_dmabuf_no_client");
                        }
                    }
                    Err(_already_ready) => {
                        state.dmabuf.stats.last_blocker_error = None;
                    }
                }
            }
        });
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.unregister_surface(surface);
        // Defer until Smithay has completed the destruction callback so the
        // coalesced lifecycle refresh observes the final surface tree.
        self.schedule_window_lifecycle_refresh();
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.stats.record_commit(self.dmabuf.fast_path.is_some());
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .space
                .elements()
                .find(|window| Self::window_toplevel_surface(window).as_ref() == Some(&root))
            {
                window.on_commit();
            }
        }

        handle_xdg_commit(self, surface);
        self.capture_managed_resize_commit(surface);
        crate::frame_export::request_redraw(self);
        crate::computer_use::capture_after_commit(self);
    }
}

impl BufferHandler for Brood {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

#[cfg(feature = "shm-fallback")]
impl ShmHandler for Brood {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for Brood {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf.state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        self.dmabuf.stats.attempts += 1;
        self.dmabuf.stats.last_import = Some(dmabuf_import_summary(&dmabuf));
        self.dmabuf.stats.last_error = None;

        let Some(exporter) = self.dmabuf.exporter.clone() else {
            self.dmabuf.stats.rejected += 1;
            self.dmabuf.stats.last_error = Some("no_frame_exporter".to_string());
            notifier.failed();
            return;
        };

        let result = exporter
            .borrow_mut()
            .renderer()
            .import_dmabuf(&dmabuf, None);

        match result {
            Ok(_) => {
                self.dmabuf.stats.accepted += 1;
                if let Err(err) = notifier.successful::<Self>() {
                    warn!(
                        ?err,
                        "client disconnected before dmabuf buffer creation completed"
                    );
                }
            }
            Err(err) => {
                self.dmabuf.stats.rejected += 1;
                self.dmabuf.stats.last_error = Some(crate::frame_export::sanitize_status_value(
                    &format!("{err:?}"),
                ));
                warn!(
                    ?err,
                    "failed to import client dmabuf into presentation renderer"
                );
                notifier.failed();
            }
        }
    }
}

impl DrmSyncobjHandler for Brood {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.dmabuf.explicit_sync_state.as_mut()
    }
}

fn dmabuf_import_summary(dmabuf: &Dmabuf) -> String {
    let size = dmabuf.size();
    let format = dmabuf.format();
    let strides = dmabuf
        .strides()
        .map(|stride| stride.to_string())
        .collect::<Vec<_>>()
        .join(".");
    let offsets = dmabuf
        .offsets()
        .map(|offset| offset.to_string())
        .collect::<Vec<_>>()
        .join(".");
    let node = dmabuf
        .node()
        .map_or_else(|| "none".to_string(), |node| format!("{node:?}"));

    crate::frame_export::sanitize_status_value(&format!(
        "{}x{}:{:?}:{:?}:planes{}:strides{}:offsets{}:yinvert{}:node{}",
        size.w,
        size.h,
        format.code,
        format.modifier,
        dmabuf.num_planes(),
        if strides.is_empty() { "none" } else { &strides },
        if offsets.is_empty() { "none" } else { &offsets },
        dmabuf.y_inverted(),
        node,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmabufBlockerPlan {
    None,
    ImplicitDmabuf,
    ExplicitSync,
}

const fn dmabuf_blocker_plan(
    has_dmabuf: bool,
    has_explicit_acquire_point: bool,
) -> DmabufBlockerPlan {
    if !has_dmabuf {
        return DmabufBlockerPlan::None;
    }
    if has_explicit_acquire_point {
        return DmabufBlockerPlan::ExplicitSync;
    }
    DmabufBlockerPlan::ImplicitDmabuf
}

impl XdgShellHandler for Brood {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        let active_surface = Self::window_toplevel_surface(&window);
        let focus_surface = active_surface.as_ref().map_or_else(
            || active_surface.clone(),
            |surface| self.assign_new_toplevel_surface(surface),
        );
        let location = self.allocate_toplevel_location();
        self.space.map_element(window, location, true);
        self.record_window_mapped();
        self.focus_toplevel_surface(focus_surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        drop(self.popups.track_popup(PopupKind::Xdg(surface)));
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        output: Option<wl_output::WlOutput>,
    ) {
        self.request_toplevel_fullscreen(surface.wl_surface().clone(), output);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.request_toplevel_unfullscreen(surface.wl_surface());
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }
}

impl XdgDecorationHandler for Brood {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        Self::configure_server_side_decoration(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        Self::configure_server_side_decoration(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        Self::configure_server_side_decoration(&toplevel);
    }
}

impl Brood {
    fn configure_server_side_decoration(toplevel: &ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }
}

fn handle_xdg_commit(state: &mut Brood, surface: &WlSurface) {
    if let Some(window) = state
        .space
        .elements()
        .find(|window| Brood::window_toplevel_surface(window).as_ref() == Some(surface))
        .cloned()
    {
        let initial_configure_sent = with_states(surface, |states| {
            let data = states.data_map.get::<XdgToplevelSurfaceData>()?;
            match data.lock() {
                Ok(data) => Some(data.initial_configure_sent),
                Err(err) => {
                    warn!(?err, "xdg toplevel data lock poisoned");
                    None
                }
            }
        });

        if initial_configure_sent == Some(false)
            && let Some(toplevel) = window.toplevel()
        {
            toplevel.send_configure();
        }
    }

    state.renegotiate_presentation_client_pane_size(surface);
    state.popups.commit(surface);
    if let Some(PopupKind::Xdg(popup)) = state.popups.find_popup(surface)
        && !popup.is_initial_configure_sent()
        && let Err(err) = popup.send_configure()
    {
        state.stats.xdg_configure_errors += 1;
        warn!(?err, "send initial popup configure failed");
    }
}

impl SeatHandler for Brood {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client);
    }
}

impl SelectionHandler for Brood {
    type SelectionUserData = ClipboardSelection;

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        seat: Seat<Self>,
    ) {
        self.sync_internal_clipboard_to_host(ty, source, seat);
    }

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        mime_type: String,
        fd: std::os::unix::io::OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        send_clipboard_selection(user_data, mime_type, fd);
    }
}

impl DataDeviceHandler for Brood {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ExtDataControlHandler for Brood {
    fn data_control_state(&self) -> &ExtDataControlState {
        &self.ext_data_control_state
    }
}

impl WlrDataControlHandler for Brood {
    fn data_control_state(&self) -> &WlrDataControlState {
        &self.wlr_data_control_state
    }
}

impl ClientDndGrabHandler for Brood {}

impl ServerDndGrabHandler for Brood {}

impl OutputHandler for Brood {}

impl FractionalScaleHandler for Brood {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        self.update_fractional_scale_for_surface(&surface);
    }
}

smithay::delegate_compositor!(Brood);
smithay::delegate_xdg_shell!(Brood);
smithay::delegate_xdg_decoration!(Brood);
#[cfg(feature = "shm-fallback")]
smithay::delegate_shm!(Brood);
smithay::delegate_dmabuf!(Brood);
smithay::delegate_drm_syncobj!(Brood);
smithay::delegate_seat!(Brood);
smithay::delegate_data_device!(Brood);
smithay::delegate_ext_data_control!(Brood);
smithay::delegate_data_control!(Brood);
smithay::delegate_output!(Brood);
smithay::delegate_fractional_scale!(Brood);
smithay::delegate_viewporter!(Brood);
