use std::{
    cell::RefCell,
    fmt,
    fs::OpenOptions,
    os::fd::OwnedFd,
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
#[cfg(feature = "shm-fallback")]
use smithay::backend::renderer::ImportMemWl;
use smithay::{
    backend::allocator::Fourcc,
    backend::{
        drm::{DrmDeviceFd, DrmNode},
        egl::EGLDevice,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                solid::{SolidColorBuffer, SolidColorRenderElement},
                Kind,
            },
            gles::GlesRenderer,
            ExportMem, ImportDma, ImportEgl,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    desktop::Space,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            LoopHandle,
        },
        winit::{dpi::LogicalSize, window::WindowAttributes},
    },
    utils::DeviceFd,
    utils::{Buffer as BufferCoords, Point, Rectangle, Transform},
    wayland::{
        dmabuf::DmabufFeedbackBuilder,
        drm_syncobj::{supports_syncobj_eventfd, DrmSyncobjState},
    },
};
use tracing::{info, warn};

use crate::{
    layout::{LayoutLeafKind, LayoutRect},
    state::{Manifold, PresentationStatus},
};

pub(crate) struct PresenterHandle {
    pub(crate) backend: Rc<RefCell<WinitGraphicsBackend<GlesRenderer>>>,
    pub(crate) output: Output,
    pub(crate) damage_tracker: RefCell<OutputDamageTracker>,
}

const PRESENTATION_REFRESH_MILLIHZ: i32 = 60_000;
const TEMP_AGGRESSIVE_PRESENTATION_POLL: bool = false;
const CLIENT_FRAME_DEFER: Duration = Duration::from_millis(2);
const INPUT_FRAME_DEFER_MAX: Duration = Duration::from_millis(3);
const INPUT_FRAME_DEFER_MIN: Duration = Duration::from_micros(500);

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
    let presenter = Rc::new(PresenterHandle {
        backend,
        output,
        damage_tracker: RefCell::new(damage_tracker),
    });
    state.presenter = Some(Rc::clone(&presenter));

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
                    state.reconfigure_focus_stack();
                }
                WinitEvent::Input(event) => {
                    state.process_input_event(event);
                    if let Err(err) = state.display_handle.flush_clients() {
                        warn!(?err, "flush clients after presentation input failed");
                    }
                    if !schedule_deferred_client_frame(&event_presenter, state) {
                        event_presenter.backend.borrow().window().request_redraw();
                    }
                }
                WinitEvent::Redraw => {
                    if state.presentation == PresentationStatus::Detached {
                        return;
                    }
                    if schedule_deferred_client_frame(&event_presenter, state) {
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

fn schedule_deferred_client_frame(presenter: &Rc<PresenterHandle>, state: &mut Manifold) -> bool {
    if state.presentation_redraw_deferred {
        return true;
    }
    if state.active_window().is_none() {
        return false;
    }

    let input_delay = state
        .stats
        .input_frame_defer_remaining(INPUT_FRAME_DEFER_MAX)
        .filter(|delay| *delay >= INPUT_FRAME_DEFER_MIN);
    let delay = input_delay.unwrap_or(CLIENT_FRAME_DEFER);

    state.presentation_redraw_deferred = true;
    state.stats.record_client_frame_deferred();
    let deferred_for_input = input_delay.is_some();
    if deferred_for_input {
        state.stats.record_input_frame_deferred();
    }
    let early_callbacks = send_active_client_frame_callbacks(presenter, state);
    if early_callbacks > 0 {
        state.presentation_skip_post_submit_frame_callbacks = true;
    }
    let timer_presenter = Rc::clone(presenter);
    let result =
        state
            .loop_handle
            .insert_source(Timer::from_duration(delay), move |_, _, state| {
                state.presentation_redraw_deferred = false;
                apply_pending_attach_state(&timer_presenter, state);
                if state.presentation == PresentationStatus::Attached {
                    state.stats.record_client_deferred_frame();
                    if deferred_for_input {
                        state.stats.record_input_deferred_frame();
                    }
                    if let Err(failure) = render_presenter_frame(&timer_presenter, state) {
                        record_presentation_runtime_failure(state, &failure);
                        apply_pending_attach_state(&timer_presenter, state);
                    }
                } else {
                    state.presentation_skip_post_submit_frame_callbacks = false;
                }
                TimeoutAction::Drop
            });
    if let Err(err) = result {
        state.presentation_redraw_deferred = false;
        warn!(?err, "insert deferred input presentation timer failed");
        return false;
    }

    true
}

fn render_presenter_frame(
    presenter: &Rc<PresenterHandle>,
    state: &mut Manifold,
) -> std::result::Result<(), PresentationRuntimeFailure> {
    render_presenter_frame_with_capture(presenter, state, None)
}

pub(crate) fn dump_composed_frame(
    state: &mut Manifold,
    path: &Path,
) -> std::result::Result<(), PresentationRuntimeFailure> {
    if state.presentation != PresentationStatus::Attached {
        return Err(PresentationRuntimeFailure {
            context: "capture",
            detail: "presentation_not_attached".to_string(),
        });
    }
    let presenter =
        state
            .presenter
            .as_ref()
            .cloned()
            .ok_or_else(|| PresentationRuntimeFailure {
                context: "capture",
                detail: "presentation_not_attached".to_string(),
            })?;
    render_presenter_frame_with_capture(&presenter, state, Some(path))
}

fn render_presenter_frame_with_capture(
    presenter: &Rc<PresenterHandle>,
    state: &mut Manifold,
    capture_path: Option<&Path>,
) -> std::result::Result<(), PresentationRuntimeFailure> {
    let frame_started = Instant::now();
    let mut backend = presenter.backend.borrow_mut();
    let output = presenter.output.clone();
    let mut damage_tracker = presenter.damage_tracker.borrow_mut();
    let size = backend.window_size();
    let damage = Rectangle::from_size(size);
    let mut render_space = Space::default();
    render_space.map_output(&output, (0, 0));
    for window in state
        .space
        .elements()
        .filter(|window| state.window_in_active_surface_session(window))
    {
        let loc = state
            .window_layout_rect(window)
            .map_or((0, 0), |rect| (rect.x, rect.y));
        render_space.map_element(window.clone(), loc, true);
    }
    if let Some(active_window) = state.active_window() {
        let loc = state
            .window_layout_rect(&active_window)
            .map_or((0, 0), |rect| (rect.x, rect.y));
        render_space.map_element(active_window, loc, true);
    }
    let chrome_elements = layout_chrome_elements(state);

    {
        let (renderer, mut framebuffer) = backend
            .bind()
            .map_err(|err| PresentationRuntimeFailure::new("bind_winit_backend", err))?;
        smithay::desktop::space::render_output::<_, SolidColorRenderElement, _, _>(
            &output,
            renderer,
            &mut framebuffer,
            1.0,
            0,
            [&render_space],
            &chrome_elements,
            &mut damage_tracker,
            [0.02, 0.02, 0.025, 1.0],
        )
        .map_err(|err| PresentationRuntimeFailure::new("render_output", err))?;
        if let Some(path) = capture_path {
            let mapping = renderer
                .copy_framebuffer(
                    &framebuffer,
                    Rectangle::<i32, BufferCoords>::from_size((size.w, size.h).into()),
                    Fourcc::Abgr8888,
                )
                .map_err(|err| PresentationRuntimeFailure::new("copy_framebuffer", err))?;
            let copy = renderer
                .map_texture(&mapping)
                .map_err(|err| PresentationRuntimeFailure::new("map_texture", err))?;
            image::save_buffer(
                path,
                copy,
                size.w as u32,
                size.h as u32,
                image::ColorType::Rgba8,
            )
            .map_err(|err| PresentationRuntimeFailure::new("save_buffer", err))?;
        }
    }
    backend
        .submit(Some(&[damage]))
        .map_err(|err| PresentationRuntimeFailure::new("submit_winit_frame", err))?;
    state.stats.record_frame(frame_started.elapsed());

    if state.presentation_skip_post_submit_frame_callbacks {
        state.presentation_skip_post_submit_frame_callbacks = false;
    } else {
        send_active_client_frame_callbacks(presenter, state);
    }

    backend.window().request_redraw();
    Ok(())
}

fn layout_chrome_elements(state: &Manifold) -> Vec<SolidColorRenderElement> {
    const PLACEHOLDER_FILL: [f32; 4] = [0.08, 0.12, 0.15, 0.92];
    const PLACEHOLDER_INSET: [f32; 4] = [0.12, 0.18, 0.22, 0.86];
    const BORDER: [f32; 4] = [0.22, 0.30, 0.36, 1.0];
    const ACTIVE_BORDER: [f32; 4] = [0.36, 0.64, 0.78, 1.0];

    let mut elements = Vec::new();
    for region in state.layout.resolve(state.presentation_layout_size()) {
        if region.kind == LayoutLeafKind::Placeholder {
            push_rect(&mut elements, inset(region.rect, 10), PLACEHOLDER_FILL);
            push_rect(&mut elements, inset(region.rect, 18), PLACEHOLDER_INSET);
        }

        let color = if region.active { ACTIVE_BORDER } else { BORDER };
        push_border(&mut elements, region.rect, 1, color);
    }
    elements
}

fn push_border(
    elements: &mut Vec<SolidColorRenderElement>,
    rect: LayoutRect,
    thickness: i32,
    color: [f32; 4],
) {
    let thickness = thickness.max(1);
    push_rect(
        elements,
        LayoutRect {
            h: thickness,
            ..rect
        },
        color,
    );
    push_rect(
        elements,
        LayoutRect {
            y: rect.y + rect.h - thickness,
            h: thickness,
            ..rect
        },
        color,
    );
    push_rect(
        elements,
        LayoutRect {
            w: thickness,
            ..rect
        },
        color,
    );
    push_rect(
        elements,
        LayoutRect {
            x: rect.x + rect.w - thickness,
            w: thickness,
            ..rect
        },
        color,
    );
}

fn inset(rect: LayoutRect, amount: i32) -> LayoutRect {
    let amount = amount.max(0);
    let max_inset = (rect.w / 2).min(rect.h / 2).saturating_sub(1);
    let amount = amount.min(max_inset);
    LayoutRect {
        x: rect.x + amount,
        y: rect.y + amount,
        w: rect.w - amount * 2,
        h: rect.h - amount * 2,
    }
}

fn push_rect(elements: &mut Vec<SolidColorRenderElement>, rect: LayoutRect, color: [f32; 4]) {
    if rect.w <= 0 || rect.h <= 0 {
        return;
    }
    let buffer = SolidColorBuffer::new((rect.w, rect.h), color);
    elements.push(SolidColorRenderElement::from_buffer(
        &buffer,
        Point::from((rect.x, rect.y)),
        1.0,
        1.0,
        Kind::Unspecified,
    ));
}

fn send_active_client_frame_callbacks(
    presenter: &Rc<PresenterHandle>,
    state: &mut Manifold,
) -> u64 {
    let output = presenter.output.clone();
    let active_window = state.active_window();
    let mut client_frame_callbacks = 0;
    active_window.iter().for_each(|window| {
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
    state
        .stats
        .record_client_frame_callbacks(client_frame_callbacks);
    if let Err(err) = state.display_handle.flush_clients() {
        warn!(?err, "flush clients failed");
    }

    client_frame_callbacks
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
pub(crate) struct PresentationRuntimeFailure {
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

fn apply_pending_attach_state(presenter: &Rc<PresenterHandle>, state: &mut Manifold) {
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
        dmabuf_advertisement_for, explicit_sync_disabled_reason_for,
        presentation_control_poll_interval, presentation_runtime_error_status,
        sanitize_status_value, DmabufAdvertisement, RenderNodeProbe,
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
