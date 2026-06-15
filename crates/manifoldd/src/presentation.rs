use std::{
    cell::RefCell,
    fmt,
    fs::OpenOptions,
    os::fd::OwnedFd,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
#[cfg(feature = "shm-fallback")]
use smithay::backend::renderer::ImportMemWl;
use smithay::{
    backend::{
        drm::{DrmDeviceFd, DrmNode},
        egl::EGLDevice,
        renderer::{
            ImportDma, ImportEgl, damage::OutputDamageTracker,
            element::surface::WaylandSurfaceRenderElement, gles::GlesRenderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            LoopHandle,
            timer::{TimeoutAction, Timer},
        },
        winit::{dpi::LogicalSize, window::WindowAttributes},
    },
    utils::DeviceFd,
    utils::{Rectangle, Transform},
    wayland::{
        dmabuf::DmabufFeedbackBuilder,
        drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd},
    },
};
use tracing::{info, warn};

use crate::state::{Manifold, PresentationStatus};

struct Presenter {
    backend: Rc<RefCell<WinitGraphicsBackend<GlesRenderer>>>,
    output: Output,
    damage_tracker: RefCell<OutputDamageTracker>,
}

const PRESENTATION_REFRESH_MILLIHZ: i32 = 60_000;
const TEMP_AGGRESSIVE_PRESENTATION_POLL: bool = false;

pub fn attach(loop_handle: &LoopHandle<'static, Manifold>, state: &mut Manifold) -> Result<()> {
    if state.presentation_started {
        state.request_attach();
        return Ok(());
    }

    state.stats.record_attach_request();
    let attributes = WindowAttributes::default()
        .with_title("Manifold")
        .with_inner_size(LogicalSize::new(1280.0, 800.0))
        .with_visible(true);
    let (mut backend, winit) = winit::init_from_attributes::<GlesRenderer>(attributes)
        .map_err(|err| anyhow!("init winit presentation: {err}"))?;
    let mode = Mode {
        size: backend.window_size(),
        refresh: PRESENTATION_REFRESH_MILLIHZ,
    };
    #[cfg(feature = "shm-fallback")]
    {
        let shm_formats = backend.renderer().shm_formats().collect::<Vec<_>>();
        state.dmabuf.shm_formats = shm_formats.len();
        state.shm_state.update_formats(shm_formats);
    }
    match backend.renderer().bind_wl_display(&state.display_handle) {
        Ok(()) => {
            state.dmabuf.egl_wl_bind_status = "ok".to_string();
            info!("EGL wl_display binding enabled");
        }
        Err(err) => {
            state.dmabuf.egl_wl_bind_status = sanitize_status_value(&format!("{err:?}"));
            warn!(?err, "failed to bind EGL display to nested Wayland display");
        }
    }
    initialize_dmabuf(state, &mut backend);

    let output = Output::new(
        "manifold-winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Manifold".into(),
            model: "Winit".into(),
        },
    );
    let _global = output.create_global::<Manifold>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));

    let damage_tracker = OutputDamageTracker::from_output(&output);
    let backend = Rc::new(RefCell::new(backend));
    state.dmabuf.importer = Some(Rc::clone(&backend));
    let presenter = Rc::new(Presenter {
        backend,
        output,
        damage_tracker: RefCell::new(damage_tracker),
    });

    state.presentation = PresentationStatus::Attached;
    state.presentation_started = true;
    state.presentation_last_error = None;
    state.stats.record_attach_applied();
    state.stats.attach_count += 1;
    info!("presentation attached");

    presenter.backend.borrow().window().request_redraw();

    let timer_presenter = Rc::clone(&presenter);
    loop_handle
        .insert_source(Timer::immediate(), move |_, _, state| {
            apply_pending_attach_state(&timer_presenter, state);
            if TEMP_AGGRESSIVE_PRESENTATION_POLL
                && state.presentation == PresentationStatus::Attached
            {
                if let Err(failure) = render_presenter_frame(&timer_presenter, state) {
                    record_presentation_runtime_failure(state, &failure);
                    apply_pending_attach_state(&timer_presenter, state);
                }
            }
            TimeoutAction::ToDuration(presentation_control_poll_interval(
                state.presentation,
                state.attach_requested,
            ))
        })
        .map_err(|err| anyhow!("insert presentation control timer: {err}"))?;

    let event_presenter = Rc::clone(&presenter);
    loop_handle
        .insert_source(winit, move |event, _, state| {
            apply_pending_attach_state(&event_presenter, state);

            match event {
                WinitEvent::Resized { size, .. } => {
                    event_presenter.output.change_current_state(
                        Some(Mode {
                            size,
                            refresh: PRESENTATION_REFRESH_MILLIHZ,
                        }),
                        None,
                        None,
                        None,
                    );
                }
                WinitEvent::Input(event) => state.process_input_event(event),
                WinitEvent::Redraw => {
                    if state.presentation == PresentationStatus::Detached {
                        return;
                    }

                    if let Err(failure) = render_presenter_frame(&event_presenter, state) {
                        record_presentation_runtime_failure(state, &failure);
                        apply_pending_attach_state(&event_presenter, state);
                    }
                }
                WinitEvent::CloseRequested => {
                    state.request_detach();
                    apply_pending_attach_state(&event_presenter, state);
                }
                _ => {}
            }
        })
        .map_err(|err| anyhow!("insert winit source: {err}"))?;

    Ok(())
}

fn render_presenter_frame(
    presenter: &Rc<Presenter>,
    state: &mut Manifold,
) -> std::result::Result<(), PresentationRuntimeFailure> {
    let frame_started = Instant::now();
    let mut backend = presenter.backend.borrow_mut();
    let output = presenter.output.clone();
    let mut damage_tracker = presenter.damage_tracker.borrow_mut();
    let size = backend.window_size();
    let damage = Rectangle::from_size(size);

    {
        let (renderer, mut framebuffer) = backend
            .bind()
            .map_err(|err| PresentationRuntimeFailure::new("bind_winit_backend", err))?;
        smithay::desktop::space::render_output::<
            _,
            WaylandSurfaceRenderElement<GlesRenderer>,
            _,
            _,
        >(
            &output,
            renderer,
            &mut framebuffer,
            1.0,
            0,
            [&state.space],
            &[],
            &mut damage_tracker,
            [0.02, 0.02, 0.025, 1.0],
        )
        .map_err(|err| PresentationRuntimeFailure::new("render_output", err))?;
    }
    backend
        .submit(Some(&[damage]))
        .map_err(|err| PresentationRuntimeFailure::new("submit_winit_frame", err))?;
    state.stats.record_frame(frame_started.elapsed());

    let mut client_frame_callbacks = 0;
    state.space.elements().for_each(|window| {
        window.send_frame(
            &output,
            state.start_time.elapsed(),
            Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        );
        client_frame_callbacks += 1;
        if let Some(feedback) = state.dmabuf.default_feedback.as_ref() {
            window.send_dmabuf_feedback(&output, |_, _| Some(output.clone()), |_, _| feedback);
        }
    });
    state.stats.client_frame_callbacks += client_frame_callbacks;
    state.refresh_window_state();
    if let Err(err) = state.display_handle.flush_clients() {
        warn!(?err, "flush clients failed");
    }

    backend.window().request_redraw();
    Ok(())
}

fn initialize_dmabuf(state: &mut Manifold, backend: &mut WinitGraphicsBackend<GlesRenderer>) {
    let dmabuf_formats = backend.renderer().dmabuf_formats();
    state.dmabuf.advertised_formats = dmabuf_formats.indexset().len();
    let advertisement = dmabuf_advertisement_for(state.dmabuf.advertised_formats, None);
    if let DmabufAdvertisement::Disabled { reason } = advertisement {
        state.dmabuf.disabled_reason = Some(reason);
        state.dmabuf.explicit_sync_disabled_reason =
            explicit_sync_disabled_reason_for(advertisement);
        #[cfg(not(feature = "shm-fallback"))]
        panic!("renderer reported no importable dmabuf texture formats");
        #[cfg(feature = "shm-fallback")]
        warn!("renderer reported no dmabuf texture formats; linux dmabuf global disabled");
        #[cfg(feature = "shm-fallback")]
        return;
    }
    state.dmabuf.disabled_reason = None;

    let render_node = EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node());

    match render_node {
        Ok(Some(node)) => {
            debug_assert_eq!(
                dmabuf_advertisement_for(
                    state.dmabuf.advertised_formats,
                    Some(RenderNodeProbe::Available),
                ),
                DmabufAdvertisement::Version4,
            );
            let feedback =
                match DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats.clone()).build() {
                    Ok(feedback) => feedback,
                    Err(err) => {
                        if let DmabufAdvertisement::Version3 { disabled_reason } =
                            dmabuf_advertisement_for(
                                state.dmabuf.advertised_formats,
                                Some(RenderNodeProbe::DefaultFeedbackFailed),
                            )
                        {
                            state.dmabuf.disabled_reason = Some(disabled_reason);
                            state.dmabuf.explicit_sync_disabled_reason = disabled_reason;
                        }
                        state.dmabuf.default_feedback = None;
                        state.dmabuf.render_node = Some(format!("{node:?}"));
                        warn!(
                            ?err,
                            render_node = ?state.dmabuf.render_node,
                            "failed to build linux dmabuf default feedback; falling back to v3"
                        );
                        initialize_dmabuf_v3(state, dmabuf_formats);
                        return;
                    }
                };
            let global = state
                .dmabuf
                .state
                .create_global_with_default_feedback::<Manifold>(&state.display_handle, &feedback);
            state.dmabuf.global = Some(global);
            state.dmabuf.default_feedback = Some(feedback);
            state.dmabuf.protocol_version = Some(4);
            state.dmabuf.render_node = Some(format!("{node:?}"));
            initialize_explicit_sync(state, node);
            info!(
                render_node = ?state.dmabuf.render_node,
                formats = state.dmabuf.advertised_formats,
                "linux dmabuf v4 advertised"
            );
        }
        Ok(None) => {
            if let DmabufAdvertisement::Version3 { disabled_reason } = dmabuf_advertisement_for(
                state.dmabuf.advertised_formats,
                Some(RenderNodeProbe::Unavailable),
            ) {
                state.dmabuf.disabled_reason = Some(disabled_reason);
                state.dmabuf.explicit_sync_disabled_reason = disabled_reason;
            }
            warn!("failed to query EGL render node; falling back to linux dmabuf v3");
            initialize_dmabuf_v3(state, dmabuf_formats);
        }
        Err(err) => {
            if let DmabufAdvertisement::Version3 { disabled_reason } = dmabuf_advertisement_for(
                state.dmabuf.advertised_formats,
                Some(RenderNodeProbe::QueryFailed),
            ) {
                state.dmabuf.disabled_reason = Some(disabled_reason);
                state.dmabuf.explicit_sync_disabled_reason = disabled_reason;
            }
            warn!(
                ?err,
                "failed to query EGL device; falling back to linux dmabuf v3"
            );
            initialize_dmabuf_v3(state, dmabuf_formats);
        }
    }
}

fn initialize_explicit_sync(state: &mut Manifold, render_node: DrmNode) {
    let Some(path) = render_node.dev_path() else {
        state.dmabuf.explicit_sync_disabled_reason = "drm_render_node_path_unavailable";
        warn!(
            render_node = ?render_node,
            "render node has no device path; linux drm syncobj global disabled"
        );
        return;
    };

    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(err) => {
            state.dmabuf.explicit_sync_disabled_reason = "drm_render_node_open_failed";
            warn!(
                ?err,
                path = %path.display(),
                "failed to open render node; linux drm syncobj global disabled"
            );
            return;
        }
    };

    let owned_fd: OwnedFd = file.into();
    let import_device = DrmDeviceFd::new(DeviceFd::from(owned_fd));
    if !supports_syncobj_eventfd(&import_device) {
        state.dmabuf.explicit_sync_disabled_reason = "drm_syncobj_eventfd_unsupported";
        warn!(
            path = %path.display(),
            "render node does not support syncobj_eventfd; linux drm syncobj global disabled"
        );
        return;
    }

    state.dmabuf.explicit_sync_state = Some(DrmSyncobjState::new::<Manifold>(
        &state.display_handle,
        import_device,
    ));
    state.dmabuf.explicit_sync_protocol_version = Some(1);
    state.dmabuf.explicit_sync_disabled_reason = "none";
    info!(
        path = %path.display(),
        "linux drm syncobj v1 advertised"
    );
}

fn initialize_dmabuf_v3(
    state: &mut Manifold,
    dmabuf_formats: smithay::backend::allocator::format::FormatSet,
) {
    let global = state
        .dmabuf
        .state
        .create_global::<Manifold>(&state.display_handle, dmabuf_formats);
    state.dmabuf.global = Some(global);
    state.dmabuf.protocol_version = Some(3);
    info!(
        formats = state.dmabuf.advertised_formats,
        "linux dmabuf v3 advertised"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderNodeProbe {
    Available,
    Unavailable,
    QueryFailed,
    DefaultFeedbackFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmabufAdvertisement {
    Disabled { reason: &'static str },
    Version3 { disabled_reason: &'static str },
    Version4,
}

fn dmabuf_advertisement_for(
    format_count: usize,
    render_node: Option<RenderNodeProbe>,
) -> DmabufAdvertisement {
    if format_count == 0 {
        return DmabufAdvertisement::Disabled {
            reason: "no_dmabuf_texture_formats",
        };
    }

    match render_node {
        Some(RenderNodeProbe::Available) => DmabufAdvertisement::Version4,
        Some(RenderNodeProbe::Unavailable) | None => DmabufAdvertisement::Version3 {
            disabled_reason: "v3_no_render_node",
        },
        Some(RenderNodeProbe::QueryFailed) => DmabufAdvertisement::Version3 {
            disabled_reason: "v3_egl_device_error",
        },
        Some(RenderNodeProbe::DefaultFeedbackFailed) => DmabufAdvertisement::Version3 {
            disabled_reason: "v3_default_feedback_error",
        },
    }
}

fn explicit_sync_disabled_reason_for(advertisement: DmabufAdvertisement) -> &'static str {
    match advertisement {
        DmabufAdvertisement::Disabled { reason } => reason,
        DmabufAdvertisement::Version3 { disabled_reason } => disabled_reason,
        DmabufAdvertisement::Version4 => "none",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PresentationRuntimeFailure {
    context: &'static str,
    detail: String,
}

impl PresentationRuntimeFailure {
    fn new(context: &'static str, error: impl fmt::Debug) -> Self {
        Self {
            context,
            detail: format!("{error:?}"),
        }
    }

    fn status_value(&self) -> String {
        presentation_runtime_error_status(self.context, &self.detail)
    }
}

fn presentation_runtime_error_status(context: &str, detail: &str) -> String {
    sanitize_status_value(&format!("runtime_{context}:{detail}"))
}

fn record_presentation_runtime_failure(state: &mut Manifold, failure: &PresentationRuntimeFailure) {
    state.presentation_runtime_failures += 1;
    state.presentation_last_error = Some(failure.status_value());
    warn!(
        context = failure.context,
        error = %failure.detail,
        "presentation redraw failed; detaching presentation window"
    );
    state.request_detach();
}

pub(crate) fn sanitize_status_value(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':' | '[' | ']' | '"') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn apply_pending_attach_state(presenter: &Rc<Presenter>, state: &mut Manifold) {
    if state.detach_requested {
        state.detach_requested = false;
        presenter.backend.borrow().window().set_visible(false);
        state.space.unmap_output(&presenter.output);
        state.presentation = PresentationStatus::Detached;
        state.stats.record_detach_applied();
        state.stats.detach_count += 1;
        info!("presentation detached");
    }

    if state.attach_requested {
        state.attach_requested = false;
        presenter.backend.borrow().window().set_visible(true);
        state.space.map_output(&presenter.output, (0, 0));
        state.presentation = PresentationStatus::Attached;
        state.stats.record_attach_applied();
        state.stats.attach_count += 1;
        presenter.backend.borrow().window().request_redraw();
        info!("presentation reattached");
    }
}

fn presentation_control_poll_interval(
    presentation: PresentationStatus,
    attach_requested: bool,
) -> Duration {
    if TEMP_AGGRESSIVE_PRESENTATION_POLL && presentation == PresentationStatus::Attached {
        return Duration::ZERO;
    }
    if presentation == PresentationStatus::Detached || attach_requested {
        Duration::from_millis(8)
    } else {
        Duration::from_millis(50)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DmabufAdvertisement, RenderNodeProbe, dmabuf_advertisement_for,
        explicit_sync_disabled_reason_for, presentation_control_poll_interval,
        presentation_runtime_error_status, sanitize_status_value,
    };

    #[test]
    fn dmabuf_is_disabled_when_renderer_reports_no_formats() {
        assert_eq!(
            dmabuf_advertisement_for(0, None),
            DmabufAdvertisement::Disabled {
                reason: "no_dmabuf_texture_formats",
            },
        );
        assert_eq!(
            dmabuf_advertisement_for(0, Some(RenderNodeProbe::Available)),
            DmabufAdvertisement::Disabled {
                reason: "no_dmabuf_texture_formats",
            },
        );
    }

    #[test]
    fn dmabuf_uses_v4_when_formats_and_render_node_are_available() {
        assert_eq!(
            dmabuf_advertisement_for(3, Some(RenderNodeProbe::Available)),
            DmabufAdvertisement::Version4,
        );
    }

    #[test]
    fn dmabuf_falls_back_to_v3_when_render_node_is_missing_or_errors() {
        assert_eq!(
            dmabuf_advertisement_for(3, Some(RenderNodeProbe::Unavailable)),
            DmabufAdvertisement::Version3 {
                disabled_reason: "v3_no_render_node",
            },
        );
        assert_eq!(
            dmabuf_advertisement_for(3, Some(RenderNodeProbe::QueryFailed)),
            DmabufAdvertisement::Version3 {
                disabled_reason: "v3_egl_device_error",
            },
        );
        assert_eq!(
            dmabuf_advertisement_for(3, Some(RenderNodeProbe::DefaultFeedbackFailed)),
            DmabufAdvertisement::Version3 {
                disabled_reason: "v3_default_feedback_error",
            },
        );
    }

    #[test]
    fn explicit_sync_disabled_reason_tracks_dmabuf_advertisement() {
        assert_eq!(
            explicit_sync_disabled_reason_for(dmabuf_advertisement_for(0, None)),
            "no_dmabuf_texture_formats",
        );
        assert_eq!(
            explicit_sync_disabled_reason_for(dmabuf_advertisement_for(
                3,
                Some(RenderNodeProbe::Unavailable),
            )),
            "v3_no_render_node",
        );
        assert_eq!(
            explicit_sync_disabled_reason_for(dmabuf_advertisement_for(
                3,
                Some(RenderNodeProbe::QueryFailed),
            )),
            "v3_egl_device_error",
        );
        assert_eq!(
            explicit_sync_disabled_reason_for(dmabuf_advertisement_for(
                3,
                Some(RenderNodeProbe::DefaultFeedbackFailed),
            )),
            "v3_default_feedback_error",
        );
        assert_eq!(
            explicit_sync_disabled_reason_for(dmabuf_advertisement_for(
                3,
                Some(RenderNodeProbe::Available),
            )),
            "none",
        );
    }

    #[test]
    fn status_values_are_shell_token_safe() {
        assert_eq!(
            sanitize_status_value("EglExtensionNotSupported([\"EGL_WL_bind_wayland_display\"])"),
            "EglExtensionNotSupported_[\"EGL_WL_bind_wayland_display\"]_",
        );
    }

    #[test]
    fn presentation_runtime_errors_are_shell_token_safe() {
        assert_eq!(
            presentation_runtime_error_status("render_output", "BackendError(\"lost device\")"),
            "runtime_render_output:BackendError_\"lost_device\"_",
        );
    }

    #[test]
    fn presentation_control_polling_is_fast_while_detached() {
        assert_eq!(
            presentation_control_poll_interval(crate::state::PresentationStatus::Detached, false)
                .as_millis(),
            8,
        );

        assert_eq!(
            presentation_control_poll_interval(crate::state::PresentationStatus::Attached, false)
                .as_millis(),
            50,
        );

        assert_eq!(
            presentation_control_poll_interval(crate::state::PresentationStatus::Attached, true)
                .as_millis(),
            8,
        );
    }
}
