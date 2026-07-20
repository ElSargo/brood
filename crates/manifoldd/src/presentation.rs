#![allow(
    clippy::if_same_then_else,
    clippy::needless_for_each,
    clippy::needless_pass_by_ref_mut,
    reason = "presentation lifecycle branches and Smithay callback signatures retain framework-shaped control flow"
)]

use std::{
    cell::RefCell,
    fmt,
    fs::OpenOptions,
    os::fd::OwnedFd,
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
#[cfg(feature = "shm-fallback")]
use smithay::backend::renderer::ImportMemWl;
use smithay::{
    backend::SwapBuffersError,
    backend::allocator::{Buffer as AllocatorBuffer, Fourcc, Modifier, dmabuf::Dmabuf},
    backend::{
        drm::{DrmDeviceFd, DrmNode},
        egl::EGLDevice,
        renderer::{
            ExportMem, ImportAll, ImportDma, Renderer,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, Kind,
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::WaylandSurfaceRenderElement,
            },
        },
    },
    desktop::{Space, Window, space::SpaceElement},
    output::{Mode, Output, PhysicalProperties, Scale as OutputScale, Subpixel},
    reexports::calloop::{
        LoopHandle,
        timer::{TimeoutAction, Timer},
    },
    utils::DeviceFd,
    utils::{
        Buffer as BufferCoords, IsAlive, Logical, Physical, Point, Rectangle, Scale, Size,
        Transform,
    },
    wayland::{
        dmabuf::DmabufFeedbackBuilder,
        drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd},
    },
};
use tracing::{info, warn};

use crate::{
    geometry::capture_extent,
    layout::{LayoutItemKind, LayoutRect},
    state::{Manifold, PresentationStatus, SESSION_OUTPUT_REFRESH_MILLIHZ},
    winit_presenter::{
        PresenterBackend, PresenterEvent, PresenterEventSource, PresenterRuntime,
        SharedPresenterRuntime, WaylandEglBind, axis_frame, normalize_host_scale_factor,
    },
};

pub struct PresenterHandle {
    pub(crate) runtime: SharedPresenterRuntime,
    pub(crate) output: Output,
    pub(crate) damage_tracker: RefCell<OutputDamageTracker>,
}

fn output_scale_for_host(scale_factor: f64) -> OutputScale {
    OutputScale::Fractional(normalize_host_scale_factor(scale_factor))
}

fn apply_presenter_created(presenter: &Rc<PresenterHandle>, state: &mut Manifold) {
    let mut runtime = presenter.runtime.borrow_mut();
    let Some(backend) = runtime.backend.as_mut() else {
        return;
    };
    state.dmabuf.importer = Some(Rc::clone(&presenter.runtime));
    let mode = Mode {
        size: backend.window_size(),
        refresh: SESSION_OUTPUT_REFRESH_MILLIHZ,
    };
    presenter.output.change_current_state(
        Some(mode),
        None,
        Some(output_scale_for_host(backend.scale_factor())),
        None,
    );
    presenter.output.set_preferred(mode);
    state.refresh_fractional_scales();

    #[cfg(feature = "shm-fallback")]
    {
        let shm_formats = backend.renderer().shm_formats().collect::<Vec<_>>();
        state.dmabuf.shm_formats = shm_formats.len();
        state.shm_state.update_formats(shm_formats);
    }
    match backend.bind_wl_display(&state.display_handle) {
        Ok(WaylandEglBind::RendererDisplay) => {
            state.dmabuf.egl_wl_bind_status = "renderer".to_string();
            info!("EGL wl_display binding enabled through presentation renderer display");
        }
        Ok(WaylandEglBind::Unsupported) => {
            state.dmabuf.egl_wl_bind_status = "unsupported".to_string();
            info!("legacy EGL wl_display binding unavailable; continuing with linux dmabuf");
        }
        Err(err) => {
            state.dmabuf.egl_wl_bind_status = sanitize_status_value(&format!("{err:?}"));
            warn!(?err, "failed to bind EGL display to nested Wayland display");
        }
    }
    if state.dmabuf.protocol_version.is_none() {
        initialize_dmabuf(state, backend);
    }

    state.space.map_output(&presenter.output, (0, 0));
    state.presentation = PresentationStatus::Attached;
    state.presentation_last_error = None;
    state.attach_requested = false;
    state.stats.record_attach_applied();
    state.stats.attach_count += 1;
    backend.request_redraw();
    if let Some(activation_token) = state.take_pending_presentation_focus_activation_token() {
        let token = (!activation_token.is_empty()).then_some(activation_token.as_str());
        backend.focus_window(token);
    }
    info!("presentation attached");
}

const TEMP_AGGRESSIVE_PRESENTATION_POLL: bool = false;
const CLIENT_FRAME_DEFER: Duration = Duration::from_millis(2);
const INPUT_FRAME_DEFER_MAX: Duration = Duration::from_millis(3);
const INPUT_FRAME_DEFER_MIN: Duration = Duration::from_micros(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresentationTarget {
    Winit,
    Offscreen,
}

pub fn attach(loop_handle: &LoopHandle<'static, Manifold>, state: &mut Manifold) -> Result<()> {
    attach_target(loop_handle, state, PresentationTarget::Winit)
}

pub fn attach_offscreen(
    loop_handle: &LoopHandle<'static, Manifold>,
    state: &mut Manifold,
) -> Result<()> {
    attach_target(loop_handle, state, PresentationTarget::Offscreen)
}

fn attach_target(
    loop_handle: &LoopHandle<'static, Manifold>,
    state: &mut Manifold,
    target: PresentationTarget,
) -> Result<()> {
    if state.presentation_started {
        state.request_attach();
        return Ok(());
    }

    if !state.attach_requested {
        state.restore_last_interacted_surface_session();
        state.stats.record_attach_request();
    }
    let mode = Mode {
        size: (1280, 800).into(),
        refresh: SESSION_OUTPUT_REFRESH_MILLIHZ,
    };
    let runtime = match target {
        PresentationTarget::Winit => Rc::new(RefCell::new(PresenterRuntime::new())),
        PresentationTarget::Offscreen => {
            Rc::new(RefCell::new(PresenterRuntime::new_offscreen(mode.size)))
        }
    };
    let output = state.session_output.clone();
    let initial_scale =
        (target == PresentationTarget::Offscreen).then_some(OutputScale::Integer(1));
    output.change_current_state(Some(mode), None, initial_scale, Some((0, 0).into()));
    output.set_preferred(mode);
    state.refresh_fractional_scales();

    let damage_tracker = OutputDamageTracker::from_output(&output);
    state.dmabuf.importer = Some(Rc::clone(&runtime));
    let presenter = Rc::new(PresenterHandle {
        runtime: Rc::clone(&runtime),
        output,
        damage_tracker: RefCell::new(damage_tracker),
    });
    state.presenter = Some(Rc::clone(&presenter));

    state.presentation_started = true;

    let timer_presenter = Rc::clone(&presenter);
    loop_handle
        .insert_source(Timer::immediate(), move |_, (), state| {
            apply_pending_attach_state(&timer_presenter, state);
            if target == PresentationTarget::Offscreen {
                timer_presenter.runtime.borrow_mut().ensure_offscreen();
                drain_pending_presenter_events(&timer_presenter, state);
                if offscreen_frame_requested(&timer_presenter, state)
                    && let Err(failure) = render_presenter_frame(&timer_presenter, state)
                {
                    record_presentation_runtime_failure(state, &failure);
                    apply_pending_attach_state(&timer_presenter, state);
                }
            }
            if TEMP_AGGRESSIVE_PRESENTATION_POLL
                && state.presentation == PresentationStatus::Attached
                && let Err(failure) = render_presenter_frame(&timer_presenter, state)
            {
                record_presentation_runtime_failure(state, &failure);
                apply_pending_attach_state(&timer_presenter, state);
            }
            TimeoutAction::ToDuration(presentation_control_poll_interval(
                state.presentation,
                state.attach_requested,
            ))
        })
        .map_err(|err| anyhow!("insert presentation control timer: {err}"))?;

    if target == PresentationTarget::Winit {
        let event_source = PresenterEventSource::new(Rc::clone(&runtime))
            .map_err(|err| anyhow!("init winit presentation event loop: {err}"))?;
        let event_presenter = Rc::clone(&presenter);
        loop_handle
            .insert_source(event_source, move |event, (), state| {
                apply_pending_attach_state(&event_presenter, state);
                handle_presenter_event(&event_presenter, state, event);
            })
            .map_err(|err| anyhow!("insert winit source: {err}"))?;
    }

    Ok(())
}

fn drain_pending_presenter_events(presenter: &Rc<PresenterHandle>, state: &mut Manifold) {
    let events = presenter
        .runtime
        .borrow_mut()
        .pending_events
        .drain(..)
        .collect::<Vec<_>>();
    for event in events {
        handle_presenter_event(presenter, state, event);
    }
}

fn offscreen_frame_requested(presenter: &Rc<PresenterHandle>, state: &Manifold) -> bool {
    if state.presentation != PresentationStatus::Attached {
        return false;
    }
    presenter
        .runtime
        .borrow()
        .backend
        .as_ref()
        .is_some_and(PresenterBackend::take_redraw_requested)
}

fn handle_presenter_event(
    event_presenter: &Rc<PresenterHandle>,
    state: &mut Manifold,
    event: PresenterEvent,
) {
    match event {
        PresenterEvent::Created => {
            if state.presentation != PresentationStatus::Attached || state.attach_requested {
                apply_presenter_created(event_presenter, state);
                if state.presentation == PresentationStatus::Attached
                    && let Err(failure) = render_presenter_frame(event_presenter, state)
                {
                    record_presentation_runtime_failure(state, &failure);
                    apply_pending_attach_state(event_presenter, state);
                }
            }
        }
        PresenterEvent::CreationFailed(error) => {
            state.record_presentation_attach_failure(&error);
        }
        PresenterEvent::Resized { size, scale_factor } => {
            event_presenter.output.change_current_state(
                Some(Mode {
                    size,
                    refresh: SESSION_OUTPUT_REFRESH_MILLIHZ,
                }),
                None,
                Some(output_scale_for_host(scale_factor)),
                None,
            );
            state.refresh_fractional_scales();
            state.reconfigure_focus_stack();
        }
        PresenterEvent::ScaleFactorChanged { scale_factor } => {
            event_presenter.output.change_current_state(
                None,
                None,
                Some(output_scale_for_host(scale_factor)),
                None,
            );
            state.refresh_fractional_scales();
            state.reconfigure_focus_stack();
            if let Some(backend) = event_presenter.runtime.borrow().backend.as_ref() {
                backend.request_redraw();
            }
        }
        PresenterEvent::Keyboard {
            key_code,
            state: key_state,
            time,
        } => {
            state.forward_keyboard_key(key_code, key_state, time);
            if let Err(err) = state.display_handle.flush_clients() {
                warn!(?err, "flush clients after presentation input failed");
            }
            if !schedule_deferred_client_frame(event_presenter, state, true)
                && let Some(backend) = event_presenter.runtime.borrow().backend.as_ref()
            {
                backend.request_redraw();
            }
        }
        PresenterEvent::PointerMotion { position, time } => {
            state.forward_pointer_motion(position, time);
            if let Err(err) = state.display_handle.flush_clients() {
                warn!(?err, "flush clients after presentation input failed");
            }
            if !schedule_deferred_client_frame(event_presenter, state, true)
                && let Some(backend) = event_presenter.runtime.borrow().backend.as_ref()
            {
                backend.request_redraw();
            }
        }
        PresenterEvent::PointerButton {
            button,
            state: button_state,
            time,
        } => {
            state.forward_pointer_button(button, button_state, time);
            if let Err(err) = state.display_handle.flush_clients() {
                warn!(?err, "flush clients after presentation input failed");
            }
            if !schedule_deferred_client_frame(event_presenter, state, true)
                && let Some(backend) = event_presenter.runtime.borrow().backend.as_ref()
            {
                backend.request_redraw();
            }
        }
        PresenterEvent::PointerAxis {
            horizontal_amount,
            vertical_amount,
            horizontal_v120,
            vertical_v120,
            source,
            time,
        } => {
            state.forward_pointer_axis(axis_frame(
                horizontal_amount,
                vertical_amount,
                horizontal_v120,
                vertical_v120,
                source,
                time,
            ));
            if let Err(err) = state.display_handle.flush_clients() {
                warn!(?err, "flush clients after presentation input failed");
            }
            if !schedule_deferred_client_frame(event_presenter, state, true)
                && let Some(backend) = event_presenter.runtime.borrow().backend.as_ref()
            {
                backend.request_redraw();
            }
        }
        PresenterEvent::Redraw => {
            if state.presentation == PresentationStatus::Detached {
                return;
            }
            if schedule_deferred_client_frame(event_presenter, state, false) {
                return;
            }

            if let Err(failure) = render_presenter_frame(event_presenter, state) {
                record_presentation_runtime_failure(state, &failure);
                apply_pending_attach_state(event_presenter, state);
            }
        }
        PresenterEvent::CloseRequested => {
            state.request_os_window_close();
        }
        PresenterEvent::Focus { focused } => {
            state.set_presentation_window_focused(focused);
        }
    }
}

pub fn request_presenter_redraw_after_client_commit(state: &mut Manifold) {
    if state.presentation != PresentationStatus::Attached {
        return;
    }
    if let Some(fast_path) = state.dmabuf.fast_path.as_mut()
        && fast_path.in_flight_sequence.is_some()
    {
        fast_path.pending_redraw = true;
        return;
    }
    if state.presentation_visible_window_count() == 0 {
        return;
    }
    let Some(presenter) = state.presenter.clone() else {
        return;
    };
    if state.presentation_redraw_deferred {
        return;
    }
    let offscreen = presenter
        .runtime
        .borrow()
        .backend
        .as_ref()
        .is_some_and(|backend| backend.target_name() == "offscreen");
    if offscreen && schedule_deferred_client_frame(&presenter, state, false) {
        return;
    }
    if let Some(backend) = presenter.runtime.borrow().backend.as_ref() {
        backend.request_redraw();
    }
}

pub fn request_external_client_redraw(state: &mut Manifold) {
    let Some(presenter) = state.presenter.clone() else {
        return;
    };
    let offscreen = presenter
        .runtime
        .borrow()
        .backend
        .as_ref()
        .is_some_and(|backend| backend.target_name() == "offscreen");
    if offscreen && schedule_deferred_client_frame(&presenter, state, false) {
        return;
    }
    if let Some(backend) = presenter.runtime.borrow().backend.as_ref() {
        backend.request_redraw();
    }
}

fn schedule_deferred_client_frame(
    presenter: &Rc<PresenterHandle>,
    state: &mut Manifold,
    send_early_callbacks: bool,
) -> bool {
    if state.presentation_redraw_deferred {
        return true;
    }
    if state.presentation_visible_window_count() == 0 {
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
    if send_early_callbacks {
        let early_callbacks = send_active_client_frame_callbacks(presenter, state);
        if early_callbacks > 0 {
            state.presentation_skip_post_submit_frame_callbacks = true;
        }
    }
    let timer_presenter = Rc::clone(presenter);
    let result =
        state
            .loop_handle
            .insert_source(Timer::from_duration(delay), move |_, (), state| {
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

pub fn dump_composed_frame(
    state: &mut Manifold,
    path: &Path,
) -> std::result::Result<(), PresentationRuntimeFailure> {
    if state.presentation != PresentationStatus::Attached {
        return Err(PresentationRuntimeFailure {
            context: "capture",
            detail: "presentation_not_attached".to_string(),
        });
    }
    let presenter = state
        .presenter
        .clone()
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
    let mut runtime = presenter.runtime.borrow_mut();
    let Some(backend) = runtime.backend.as_mut() else {
        return Ok(());
    };
    if capture_path.is_none() && state.dmabuf.fast_path.is_some() {
        if let Some(fast_path) = state.dmabuf.fast_path.as_mut()
            && fast_path.in_flight_sequence.is_some()
        {
            fast_path.pending_redraw = true;
            return Ok(());
        }
        render_external_client_scene(backend, state)?;
        state.stats.record_frame(frame_started.elapsed());
        return Ok(());
    }
    let output = presenter.output.clone();
    let mut damage_tracker = presenter.damage_tracker.borrow_mut();
    let size = backend.window_size();
    let damage = Rectangle::from_size(size);
    let mut render_space: Space<MarkedWindow> = Space::default();
    render_space.map_output(&output, (0, 0));
    for (index, window) in state
        .space
        .elements()
        .filter(|window| state.window_visible_in_presentation(window))
        .enumerate()
    {
        let loc = state
            .window_layout_rect(window)
            .map_or((0, 0), |rect| (rect.x, rect.y));
        render_space.map_element(marked_window(state, window, index), loc, true);
    }
    if let Some(active_window) = state.active_presentation_window() {
        let loc = state
            .window_layout_rect(&active_window)
            .map_or((0, 0), |rect| (rect.x, rect.y));
        render_space.map_element(marked_window(state, &active_window, 0), loc, true);
    }
    let output_scale = Scale::from(output.current_scale().fractional_scale());
    let chrome_elements = layout_chrome_elements(state, output_scale);

    let render_sync = {
        let (renderer, mut framebuffer) = backend
            .bind()
            .map_err(|err| PresentationRuntimeFailure::new("bind_winit_backend", err))?;
        let result = smithay::desktop::space::render_output::<_, SolidColorRenderElement, _, _>(
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
            let capture_width = capture_extent("width", size.w)
                .map_err(|err| PresentationRuntimeFailure::new("capture_dimensions", err))?;
            let capture_height = capture_extent("height", size.h)
                .map_err(|err| PresentationRuntimeFailure::new("capture_dimensions", err))?;
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
                capture_width,
                capture_height,
                image::ColorType::Rgba8,
            )
            .map_err(|err| PresentationRuntimeFailure::new("save_buffer", err))?;
        }
        result.sync
    };
    render_sync
        .wait()
        .map_err(|err| PresentationRuntimeFailure::new("wait_composed_frame", err))?;
    let submitted = match backend.submit(Some(&[damage])) {
        Ok(()) => true,
        Err(err) => {
            let failure = PresentationRuntimeFailure::new("submit_winit_frame", &err);
            if matches!(err, SwapBuffersError::ContextLost(_)) {
                backend.reset_target_after_context_loss();
            }
            if capture_path.is_some() {
                warn!(
                    detail = %failure.detail,
                    "presentation submit failed after composed frame capture; keeping capture result"
                );
                false
            } else {
                return Err(failure);
            }
        }
    };
    state.stats.record_frame(frame_started.elapsed());

    let external_client_active = state.dmabuf.fast_path.is_some();
    if !submitted {
        state.presentation_skip_post_submit_frame_callbacks = false;
    } else if state.presentation_skip_post_submit_frame_callbacks {
        state.presentation_skip_post_submit_frame_callbacks = false;
    } else if !external_client_active {
        send_active_client_frame_callbacks(presenter, state);
    }

    if !external_client_active {
        backend.request_redraw();
    }
    Ok(())
}

#[derive(Clone)]
struct ExternalWindowFrame {
    window_id: u64,
    rect: LayoutRect,
    active: bool,
    element: MarkedWindow,
}

fn render_external_client_scene(
    backend: &mut PresenterBackend,
    state: &mut Manifold,
) -> std::result::Result<(), PresentationRuntimeFailure> {
    let active_window = state.active_presentation_window();
    let mut windows = state
        .space
        .elements()
        .filter(|window| state.window_visible_in_presentation(window))
        .enumerate()
        .filter_map(|(index, window)| {
            let rect = state.window_layout_rect(window)?;
            (rect.w > 0 && rect.h > 0).then(|| ExternalWindowFrame {
                window_id: state
                    .window_pane_id(window)
                    .unwrap_or_else(|| state.window_test_marker_id(window, index)),
                rect,
                active: active_window.as_ref() == Some(window),
                element: marked_window(state, window, index),
            })
        })
        .collect::<Vec<_>>();
    windows.sort_by_key(|window| window.active);

    if windows.len().saturating_add(2) > manifold_ipc::CRITICAL_CAPACITY {
        return Err(PresentationRuntimeFailure::new(
            "publish_window_scene",
            format!(
                "scene has {} windows but the transport batch holds at most {}",
                windows.len(),
                manifold_ipc::CRITICAL_CAPACITY.saturating_sub(2)
            ),
        ));
    }

    let mut rendered = Vec::with_capacity(windows.len());
    for window in windows {
        let size: Size<i32, Physical> = (window.rect.w.max(1), window.rect.h.max(1)).into();
        let existing = state
            .dmabuf
            .fast_path
            .as_mut()
            .and_then(|fast_path| fast_path.window_buffers.remove(&window.window_id));
        let mut dmabuf = match existing {
            Some(buffer) if buffer.size() == (size.w, size.h).into() => buffer,
            _ => backend.create_exportable_dmabuf(size).map_err(|error| {
                PresentationRuntimeFailure::new("allocate_window_dmabuf", error)
            })?,
        };
        render_window_dmabuf(backend, state, &window, &mut dmabuf)?;
        let Some(fast_path) = state.dmabuf.fast_path.as_mut() else {
            return Err(PresentationRuntimeFailure::new(
                "store_window_dmabuf",
                "fast path closed while rendering",
            ));
        };
        fast_path
            .window_buffers
            .insert(window.window_id, dmabuf.clone());
        rendered.push((window, dmabuf));
    }
    publish_window_scene(state, backend.window_size(), rendered)
}

fn render_window_dmabuf(
    backend: &mut PresenterBackend,
    state: &Manifold,
    window: &ExternalWindowFrame,
    dmabuf: &mut Dmabuf,
) -> std::result::Result<(), PresentationRuntimeFailure> {
    let size: Size<i32, Physical> = (window.rect.w.max(1), window.rect.h.max(1)).into();
    let output = Output::new(
        format!("manifold-window-{}", window.window_id),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Manifold".into(),
            model: "Client window frame".into(),
        },
    );
    let mode = Mode {
        size,
        refresh: SESSION_OUTPUT_REFRESH_MILLIHZ,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        Some(state.session_output.current_scale()),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    let mut window_space = Space::default();
    window_space.map_output(&output, (0, 0));
    window_space.map_element(window.element.clone(), (0, 0), true);

    let sync = {
        let (renderer, mut framebuffer) = backend
            .bind_dmabuf(dmabuf)
            .map_err(|error| PresentationRuntimeFailure::new("bind_window_dmabuf", error))?;
        smithay::desktop::space::render_output::<_, SolidColorRenderElement, _, _>(
            &output,
            renderer,
            &mut framebuffer,
            1.0,
            0,
            [&window_space],
            &[],
            &mut damage_tracker,
            [0.02, 0.02, 0.025, 1.0],
        )
        .map_err(|error| PresentationRuntimeFailure::new("render_window_dmabuf", error))?
        .sync
    };
    sync.wait()
        .map_err(|error| PresentationRuntimeFailure::new("wait_window_dmabuf", error))
}

fn publish_window_scene(
    state: &mut Manifold,
    scene_size: Size<i32, Physical>,
    windows: Vec<(ExternalWindowFrame, Dmabuf)>,
) -> std::result::Result<(), PresentationRuntimeFailure> {
    state.dmabuf.latest_client_buffer_sequence =
        state.dmabuf.latest_client_buffer_sequence.wrapping_add(1);
    let sequence = state.dmabuf.latest_client_buffer_sequence;
    let Some(fast_path) = state.dmabuf.fast_path.as_mut() else {
        return Ok(());
    };
    if fast_path.in_flight_sequence.is_some() {
        fast_path.pending_redraw = true;
        return Ok(());
    }
    let begin = manifold_ipc::CriticalRecord {
        opcode: manifold_ipc::FRAME_SCENE_BEGIN,
        connection_generation: fast_path.generation,
        sequence,
        arguments: [
            u64::try_from(windows.len()).unwrap_or(u64::MAX),
            u64::try_from(scene_size.w.max(1)).unwrap_or(u64::MAX),
            u64::try_from(scene_size.h.max(1)).unwrap_or(u64::MAX),
            0,
            0,
        ],
        ..manifold_ipc::CriticalRecord::default()
    };
    fast_path
        .frames
        .try_send(begin)
        .map_err(|error| PresentationRuntimeFailure::new("publish_scene_begin", error))?;
    for (window, dmabuf) in windows {
        let weak_dmabuf = dmabuf.weak();
        let resource_id = if let Some(id) = fast_path.resources.get(&weak_dmabuf) {
            *id
        } else {
            fast_path.resources.retain(|buffer, _| !buffer.is_gone());
            let id = fast_path.next_resource_id;
            fast_path.next_resource_id = id.wrapping_add(1);
            fast_path.resources.insert(weak_dmabuf, id);
            fast_path.buffers.insert(id, dmabuf);
            id
        };
        fast_path
            .frames
            .try_send(manifold_ipc::CriticalRecord {
                opcode: manifold_ipc::FRAME_WINDOW_DMABUF,
                connection_generation: fast_path.generation,
                sequence,
                arguments: [
                    window.window_id,
                    resource_id,
                    pack_i32_pair(window.rect.x, window.rect.y),
                    pack_i32_pair(window.rect.w, window.rect.h),
                    u64::from(window.active),
                ],
                ..manifold_ipc::CriticalRecord::default()
            })
            .map_err(|error| PresentationRuntimeFailure::new("publish_window_frame", error))?;
    }
    fast_path
        .frames
        .try_send(manifold_ipc::CriticalRecord {
            opcode: manifold_ipc::FRAME_SCENE_END,
            connection_generation: fast_path.generation,
            sequence,
            ..manifold_ipc::CriticalRecord::default()
        })
        .map_err(|error| PresentationRuntimeFailure::new("publish_scene_end", error))?;
    fast_path
        .frame_notifications
        .notify()
        .map_err(|error| PresentationRuntimeFailure::new("notify_window_scene", error))?;
    fast_path.in_flight_sequence = Some(sequence);
    Ok(())
}

fn pack_i32_pair(first: i32, second: i32) -> u64 {
    u64::from(u32::from_ne_bytes(first.to_ne_bytes()))
        | (u64::from(u32::from_ne_bytes(second.to_ne_bytes())) << 32)
}

fn layout_chrome_elements(state: &Manifold, scale: Scale<f64>) -> Vec<SolidColorRenderElement> {
    const PLACEHOLDER_FILL: [f32; 4] = [0.08, 0.12, 0.15, 0.92];
    const PLACEHOLDER_INSET: [f32; 4] = [0.12, 0.18, 0.22, 0.86];
    const HANDLE: [f32; 4] = [0.32, 0.36, 0.40, 0.95];
    const CHROME: [f32; 4] = [0.12, 0.14, 0.16, 0.80];
    const OVERLAY: [f32; 4] = [0.08, 0.10, 0.12, 0.62];
    const BORDER: [f32; 4] = [0.22, 0.30, 0.36, 1.0];
    const ACTIVE_BORDER: [f32; 4] = [0.36, 0.64, 0.78, 1.0];

    let mut elements = Vec::new();
    for item in state.resolved_layout_items() {
        let rect = state.resolved_layout_item_rect(*item);
        match item.kind {
            LayoutItemKind::Placeholder => {
                push_rect(&mut elements, inset(rect, 10), scale, PLACEHOLDER_FILL);
                push_rect(&mut elements, inset(rect, 18), scale, PLACEHOLDER_INSET);
            }
            LayoutItemKind::Handle => push_rect(&mut elements, rect, scale, HANDLE),
            LayoutItemKind::Chrome => push_rect(&mut elements, rect, scale, CHROME),
            LayoutItemKind::Overlay => push_rect(&mut elements, rect, scale, OVERLAY),
            LayoutItemKind::Pane { .. } => {}
        }

        let color = if state.resolved_layout_item_active(*item) {
            ACTIVE_BORDER
        } else {
            BORDER
        };
        push_border(&mut elements, rect, scale, 1, color);
    }
    elements
}

fn marked_window(state: &Manifold, window: &Window, index: usize) -> MarkedWindow {
    let marker_id = state
        .test_surface_markers
        .then(|| state.window_test_marker_id(window, index));
    MarkedWindow {
        window: window.clone(),
        marker_id,
    }
}

#[derive(Clone, Debug)]
struct MarkedWindow {
    window: Window,
    marker_id: Option<u64>,
}

impl PartialEq for MarkedWindow {
    fn eq(&self, other: &Self) -> bool {
        self.window == other.window
    }
}

impl IsAlive for MarkedWindow {
    fn alive(&self) -> bool {
        self.window.alive()
    }
}

impl SpaceElement for MarkedWindow {
    fn geometry(&self) -> Rectangle<i32, Logical> {
        self.window.geometry()
    }

    fn bbox(&self) -> Rectangle<i32, Logical> {
        self.window.bbox()
    }

    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        self.window.is_in_input_region(point)
    }

    fn z_index(&self) -> u8 {
        self.window.z_index()
    }

    fn set_activate(&self, activated: bool) {
        self.window.set_activate(activated);
    }

    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        self.window.output_enter(output, overlap);
    }

    fn output_leave(&self, output: &Output) {
        self.window.output_leave(output);
    }

    fn refresh(&self) {
        self.window.refresh();
    }
}

smithay::backend::renderer::element::render_elements! {
    TestSurfaceRenderElement<R> where R: ImportAll;
    Surface=WaylandSurfaceRenderElement<R>,
    Marker=SolidColorRenderElement,
}

impl<R> AsRenderElements<R> for MarkedWindow
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    type RenderElement = TestSurfaceRenderElement<R>;

    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        let mut elements = Vec::new();
        if let Some(marker_id) = self.marker_id {
            let marker_size = self.window.geometry().size;
            elements.extend(
                marker_render_elements(marker_id, location, marker_size, scale, alpha)
                    .into_iter()
                    .map(TestSurfaceRenderElement::Marker)
                    .map(C::from),
            );
        }
        elements.extend(
            AsRenderElements::<R>::render_elements::<WaylandSurfaceRenderElement<R>>(
                &self.window,
                renderer,
                location,
                scale,
                alpha,
            )
            .into_iter()
            .map(TestSurfaceRenderElement::Surface)
            .map(C::from),
        );

        elements
    }
}

fn marker_render_elements(
    marker_id: u64,
    location: Point<i32, Physical>,
    size: Size<i32, Logical>,
    scale: Scale<f64>,
    alpha: f32,
) -> Vec<SolidColorRenderElement> {
    let marker_size: Size<i32, Physical> =
        Size::<i32, Logical>::from((MARKER_GRID * MARKER_CELL, MARKER_GRID * MARKER_CELL))
            .to_physical_precise_round(scale);
    let physical_size: Size<i32, Physical> = size.to_physical_precise_round(scale);
    let opposite_offset = Point::<i32, Physical>::from((
        (physical_size.w - marker_size.w).max(0),
        (physical_size.h - marker_size.h).max(0),
    ));

    let mut elements = Vec::new();
    push_marker(&mut elements, location, scale, alpha, marker_id);
    if opposite_offset.x != 0 || opposite_offset.y != 0 {
        push_marker(
            &mut elements,
            location + opposite_offset,
            scale,
            alpha,
            marker_id,
        );
    }
    elements
}

fn push_marker(
    elements: &mut Vec<SolidColorRenderElement>,
    location: Point<i32, Physical>,
    scale: Scale<f64>,
    alpha: f32,
    marker_id: u64,
) {
    for row in 0..MARKER_GRID {
        for col in 0..MARKER_GRID {
            if row == 1 && col == 1 {
                push_marker_cell(
                    elements,
                    location,
                    scale,
                    alpha,
                    MarkerCell::new(col, row, 1, MARKER_ANCHOR),
                );
            } else if marker_cell_is_dark(marker_id, col, row) {
                push_marker_cell(
                    elements,
                    location,
                    scale,
                    alpha,
                    MarkerCell::new(col, row, 1, MARKER_DARK),
                );
            }
        }
    }
    push_marker_cell(
        elements,
        location,
        scale,
        alpha,
        MarkerCell::new(0, 0, MARKER_GRID, MARKER_BACKGROUND),
    );
}

fn marker_cell_is_dark(marker_id: u64, col: i32, row: i32) -> bool {
    if row == 0 || col == 0 || row == MARKER_GRID - 1 || col == MARKER_GRID - 1 {
        return true;
    }
    if (row == 1 && col == MARKER_GRID - 2) || (row == MARKER_GRID - 2 && col == 1) {
        return true;
    }
    if !(2..=6).contains(&row) || !(2..=6).contains(&col) {
        return false;
    }
    let bit = u64::try_from((row - 2) * 5 + (col - 2)).unwrap_or(0);
    ((marker_id >> bit) & 1) == 1
}

const MARKER_GRID: i32 = 9;
const MARKER_CELL: i32 = 5;
const MARKER_BACKGROUND: [f32; 4] = [1.0, 1.0, 1.0, 0.94];
const MARKER_DARK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
const MARKER_ANCHOR: [f32; 4] = [1.0, 0.0, 0.75, 1.0];

#[derive(Clone, Copy)]
struct MarkerCell {
    col: i32,
    row: i32,
    span: i32,
    color: [f32; 4],
}

impl MarkerCell {
    const fn new(col: i32, row: i32, span: i32, color: [f32; 4]) -> Self {
        Self {
            col,
            row,
            span,
            color,
        }
    }
}

fn push_marker_cell(
    elements: &mut Vec<SolidColorRenderElement>,
    origin: Point<i32, Physical>,
    scale: Scale<f64>,
    alpha: f32,
    cell: MarkerCell,
) {
    let offset = Point::<i32, Logical>::from((cell.col * MARKER_CELL, cell.row * MARKER_CELL))
        .to_physical_precise_round(scale);
    let size = MARKER_CELL * cell.span;
    let buffer = SolidColorBuffer::new((size, size), cell.color);
    elements.push(SolidColorRenderElement::from_buffer(
        &buffer,
        origin + offset,
        scale,
        alpha,
        Kind::Unspecified,
    ));
}

fn push_border(
    elements: &mut Vec<SolidColorRenderElement>,
    rect: LayoutRect,
    scale: Scale<f64>,
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
        scale,
        color,
    );
    push_rect(
        elements,
        LayoutRect {
            y: rect.y.saturating_add(rect.h).saturating_sub(thickness),
            h: thickness,
            ..rect
        },
        scale,
        color,
    );
    push_rect(
        elements,
        LayoutRect {
            w: thickness,
            ..rect
        },
        scale,
        color,
    );
    push_rect(
        elements,
        LayoutRect {
            x: rect.x.saturating_add(rect.w).saturating_sub(thickness),
            w: thickness,
            ..rect
        },
        scale,
        color,
    );
}

fn inset(rect: LayoutRect, amount: i32) -> LayoutRect {
    let amount = amount.max(0);
    let max_inset = (rect.w / 2).min(rect.h / 2).saturating_sub(1);
    let amount = amount.min(max_inset);
    LayoutRect {
        x: rect.x.saturating_add(amount),
        y: rect.y.saturating_add(amount),
        w: rect.w.saturating_sub(amount.saturating_mul(2)),
        h: rect.h.saturating_sub(amount.saturating_mul(2)),
    }
}

fn push_rect(
    elements: &mut Vec<SolidColorRenderElement>,
    rect: LayoutRect,
    scale: Scale<f64>,
    color: [f32; 4],
) {
    if rect.w <= 0 || rect.h <= 0 {
        return;
    }
    let buffer = SolidColorBuffer::new((rect.w, rect.h), color);
    elements.push(SolidColorRenderElement::from_buffer(
        &buffer,
        Point::<i32, Logical>::from((rect.x, rect.y)).to_physical_precise_round(scale),
        scale,
        1.0,
        Kind::Unspecified,
    ));
}

fn send_active_client_frame_callbacks(
    presenter: &Rc<PresenterHandle>,
    state: &mut Manifold,
) -> u64 {
    let output = presenter.output.clone();
    send_client_frame_callbacks(&output, state)
}

pub fn send_external_client_frame_callbacks(state: &mut Manifold) -> u64 {
    let output = state.session_output.clone();
    send_client_frame_callbacks(&output, state)
}

fn send_client_frame_callbacks(output: &Output, state: &mut Manifold) -> u64 {
    let mut client_frame_callbacks = 0;
    let active_window = state.active_presentation_window();
    let active_surface = active_window
        .as_ref()
        .and_then(Manifold::window_toplevel_surface);
    let mut windows = Vec::new();
    if let Some(active_window) = active_window {
        windows.push(active_window);
    }
    windows.extend(
        state
            .space
            .elements()
            .filter(|window| state.window_visible_in_presentation(window))
            .filter(|window| {
                active_surface.as_ref() != Manifold::window_toplevel_surface(window).as_ref()
            })
            .cloned(),
    );
    windows.iter().for_each(|window| {
        window.send_frame(
            output,
            state.start_time.elapsed(),
            Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        );
        client_frame_callbacks += 1;
        if let Some(feedback) = state.dmabuf.default_feedback.as_ref() {
            window.send_dmabuf_feedback(output, |_, _| Some(output.clone()), |_, _| feedback);
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

fn initialize_dmabuf(state: &mut Manifold, backend: &mut PresenterBackend) {
    let mut dmabuf_formats = backend.renderer().dmabuf_formats();
    if std::env::var_os("MANIFOLD_DMABUF_HAPPY_PATH").is_some() {
        dmabuf_formats = dmabuf_formats
            .iter()
            .filter(|format| {
                format.modifier == Modifier::Linear || format.modifier == Modifier::Invalid
            })
            .copied()
            .collect();
    }
    state.dmabuf.advertised_formats = dmabuf_formats.indexset().len();
    let advertisement = dmabuf_advertisement_for(state.dmabuf.advertised_formats);
    if let DmabufAdvertisement::Disabled { reason } = advertisement {
        disable_dmabuf(state, reason);
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
            debug_assert_eq!(advertisement, DmabufAdvertisement::Version4);
            let feedback = match DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats).build() {
                Ok(feedback) => feedback,
                Err(err) => {
                    state.dmabuf.render_node = Some(format!("{node:?}"));
                    disable_dmabuf(state, "default_feedback_error");
                    warn!(
                        ?err,
                        render_node = ?state.dmabuf.render_node,
                        "failed to build linux dmabuf default feedback; linux dmabuf global disabled"
                    );
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
            disable_dmabuf(state, "no_render_node");
            warn!("failed to query EGL render node; linux dmabuf global disabled");
        }
        Err(err) => {
            disable_dmabuf(state, "egl_device_error");
            warn!(
                ?err,
                "failed to query EGL device; linux dmabuf global disabled"
            );
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

fn disable_dmabuf(state: &mut Manifold, reason: &'static str) {
    state.dmabuf.global = None;
    state.dmabuf.default_feedback = None;
    state.dmabuf.protocol_version = None;
    state.dmabuf.importer = None;
    state.dmabuf.disabled_reason = Some(reason);
    state.dmabuf.explicit_sync_state = None;
    state.dmabuf.explicit_sync_protocol_version = None;
    state.dmabuf.explicit_sync_disabled_reason = reason;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmabufAdvertisement {
    Disabled { reason: &'static str },
    Version4,
}

const fn dmabuf_advertisement_for(format_count: usize) -> DmabufAdvertisement {
    if format_count == 0 {
        return DmabufAdvertisement::Disabled {
            reason: "no_dmabuf_texture_formats",
        };
    }

    DmabufAdvertisement::Version4
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationRuntimeFailure {
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

pub fn sanitize_status_value(value: &str) -> String {
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
        state.reset_transient_input_state();
        presenter.runtime.borrow_mut().close_window();
        state.presentation = PresentationStatus::Detached;
        state.stats.record_detach_applied();
        state.stats.detach_count += 1;
        info!("presentation detached");
    }

    if state.attach_requested {
        let has_existing_window = presenter.runtime.borrow_mut().request_window();
        if has_existing_window {
            apply_presenter_created(presenter, state);
        }
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
