use anyhow::Result;
use smithay::{
    backend::{
        allocator::{Buffer, dmabuf::Dmabuf},
        renderer::{ImportDma, utils::on_commit_buffer_handler},
    },
    desktop::{PopupKind, Window},
    input::{Seat, SeatHandler, SeatState},
    reexports::{
        calloop::{EventLoop, Interest},
        wayland_server::{
            Client, Display, Resource,
            protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
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
        output::OutputHandler,
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
    },
};
use tracing::{info, warn};

use crate::{
    RunArgs,
    browser::BrowserLaunchOptions,
    control, presentation,
    state::{ClientState, Manifold, PresentationStatus},
};

pub fn run(args: RunArgs) -> Result<()> {
    let mut event_loop: EventLoop<'static, Manifold> = EventLoop::try_new()?;
    let display: Display<Manifold> = Display::new()?;
    let mut state = Manifold::new(&mut event_loop, display);

    control::listen(&mut event_loop, &args.control_socket)?;

    println!("WAYLAND_DISPLAY={}", state.socket_name.to_string_lossy());
    println!("MANIFOLDD_CONTROL={}", args.control_socket.display());

    if !args.headless {
        if let Err(err) = presentation::attach(&event_loop.handle(), &mut state) {
            warn!(
                ?err,
                "failed to attach initial presentation; continuing detached"
            );
            state.record_presentation_attach_failure(&err.to_string());
        }
    }

    if let Some(command) = args.command {
        state.spawn_hosted_app(&command)?;
    }

    if let Some(url) = args.browser {
        let options = BrowserLaunchOptions {
            url,
            ..BrowserLaunchOptions::default()
        };
        state.spawn_browser_pane(options)?;
    }

    info!("manifoldd compositor running");
    event_loop.run(None, &mut state, |state| {
        if state.attach_requested && !state.presentation_started {
            state.attach_requested = false;
            state.schedule_attach();
        }
    })?;

    Ok(())
}

impl CompositorHandler for Manifold {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client state")
            .compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, move |state, _dh, surface| {
            let mut acquire_point = None;
            let mut has_release_point = false;
            let maybe_dmabuf = with_states(surface, |surface_data| {
                let mut syncobj_guard = surface_data.cached_state.get::<DrmSyncobjCachedState>();
                let syncobj_state = syncobj_guard.pending();
                acquire_point.clone_from(&syncobj_state.acquire_point);
                has_release_point = syncobj_state.release_point.is_some();
                surface_data
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        BufferAssignment::Removed => None,
                    })
            });

            if let Some(dmabuf) = maybe_dmabuf {
                if has_release_point {
                    state.dmabuf.stats.explicit_sync_release_points += 1;
                }

                match dmabuf_blocker_plan(true, acquire_point.is_some()) {
                    DmabufBlockerPlan::ExplicitSync => {
                        let acquire_point =
                            acquire_point.expect("explicit sync plan has acquire point");
                        match acquire_point.generate_blocker() {
                            Ok((blocker, source)) => {
                                if let Some(client) = surface.client() {
                                    let result = state.loop_handle.insert_source(
                                        source,
                                        move |_, _, data| {
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
                            Err(err) => {
                                state.record_dmabuf_blocker_failure(&format!(
                                    "explicit_sync_generate_blocker:{err:?}"
                                ));
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
                                state.loop_handle.insert_source(source, move |_, _, data| {
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
                    Err(err) => {
                        state.record_dmabuf_blocker_failure(&format!(
                            "implicit_dmabuf_generate_blocker:{err:?}"
                        ));
                    }
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.stats
            .record_commit(self.presentation == PresentationStatus::Attached);
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .space
                .elements()
                .find(|window| Manifold::window_toplevel_surface(window).as_ref() == Some(&root))
            {
                window.on_commit();
            }
        }

        handle_xdg_commit(self, surface);
    }
}

impl BufferHandler for Manifold {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Manifold {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for Manifold {
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

        let Some(importer) = self.dmabuf.importer.clone() else {
            self.dmabuf.stats.rejected += 1;
            self.dmabuf.stats.last_error = Some("no_presentation_importer".to_string());
            notifier.failed();
            return;
        };

        let result = importer
            .borrow_mut()
            .renderer()
            .import_dmabuf(&dmabuf, None);

        match result {
            Ok(_) => {
                self.dmabuf.stats.accepted += 1;
                if let Err(err) = notifier.successful::<Manifold>() {
                    warn!(
                        ?err,
                        "client disconnected before dmabuf buffer creation completed"
                    );
                }
            }
            Err(err) => {
                self.dmabuf.stats.rejected += 1;
                self.dmabuf.stats.last_error = Some(crate::presentation::sanitize_status_value(
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

impl DrmSyncobjHandler for Manifold {
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
        .map(|node| format!("{node:?}"))
        .unwrap_or_else(|| "none".to_string());

    crate::presentation::sanitize_status_value(&format!(
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

fn dmabuf_blocker_plan(has_dmabuf: bool, has_explicit_acquire_point: bool) -> DmabufBlockerPlan {
    if !has_dmabuf {
        return DmabufBlockerPlan::None;
    }
    if has_explicit_acquire_point {
        return DmabufBlockerPlan::ExplicitSync;
    }
    DmabufBlockerPlan::ImplicitDmabuf
}

impl XdgShellHandler for Manifold {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        let active_surface = Manifold::window_toplevel_surface(&window);
        let location = self.allocate_toplevel_location();
        self.space.map_element(window, location, true);
        self.record_window_mapped();
        self.focus_toplevel_surface(active_surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
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

fn handle_xdg_commit(state: &mut Manifold, surface: &WlSurface) {
    if let Some(window) = state
        .space
        .elements()
        .find(|window| Manifold::window_toplevel_surface(window).as_ref() == Some(surface))
        .cloned()
    {
        let initial_configure_sent = with_states(surface, |states| {
            let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
                return None;
            };
            match data.lock() {
                Ok(data) => Some(data.initial_configure_sent),
                Err(err) => {
                    warn!(?err, "xdg toplevel data lock poisoned");
                    None
                }
            }
        });

        if initial_configure_sent == Some(false) {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_configure();
            }
        }
    }

    state.popups.commit(surface);
    if let Some(PopupKind::Xdg(popup)) = state.popups.find_popup(surface) {
        if !popup.is_initial_configure_sent() {
            if let Err(err) = popup.send_configure() {
                state.stats.xdg_configure_errors += 1;
                warn!(?err, "send initial popup configure failed");
            }
        }
    }
}

impl SeatHandler for Manifold {
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

impl SelectionHandler for Manifold {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Manifold {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for Manifold {}

impl ServerDndGrabHandler for Manifold {}

impl OutputHandler for Manifold {}

#[cfg(test)]
mod tests {
    use super::{DmabufBlockerPlan, dmabuf_blocker_plan};

    #[test]
    fn dmabuf_without_explicit_acquire_uses_implicit_readiness_blocker() {
        assert_eq!(
            dmabuf_blocker_plan(true, false),
            DmabufBlockerPlan::ImplicitDmabuf,
        );
    }

    #[test]
    fn explicit_acquire_point_requires_explicit_sync_blocker() {
        assert_eq!(
            dmabuf_blocker_plan(true, true),
            DmabufBlockerPlan::ExplicitSync,
        );
    }

    #[test]
    fn no_dmabuf_needs_no_dmabuf_blocker() {
        assert_eq!(dmabuf_blocker_plan(false, true), DmabufBlockerPlan::None,);
        assert_eq!(dmabuf_blocker_plan(false, false), DmabufBlockerPlan::None,);
    }
}

smithay::delegate_compositor!(Manifold);
smithay::delegate_xdg_shell!(Manifold);
smithay::delegate_shm!(Manifold);
smithay::delegate_dmabuf!(Manifold);
smithay::delegate_drm_syncobj!(Manifold);
smithay::delegate_seat!(Manifold);
smithay::delegate_data_device!(Manifold);
smithay::delegate_output!(Manifold);
