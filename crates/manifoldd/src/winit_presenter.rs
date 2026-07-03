#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::clone_on_ref_ptr,
    clippy::expect_used,
    clippy::let_underscore_must_use,
    clippy::undocumented_unsafe_blocks,
    clippy::used_underscore_binding,
    reason = "tracked in issues.md: presenter still keeps raw GL/Winit handles and FFI invariants inline"
)]
// TODO(lint-structure): wrap Winit/GL FFI state in audited safe renderer abstractions.

use std::{
    cell::{Cell, RefCell},
    env,
    ffi::c_void,
    fs,
    io::Error as IoError,
    os::fd::OwnedFd,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result as AnyResult};
use calloop::{
    EventSource, Interest, PostAction, Readiness, Token, TokenFactory, generic::Generic,
};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::{
    backend::{
        SwapBuffersError,
        allocator::{Fourcc, gbm::GbmDevice},
        drm::DrmDeviceFd,
        egl::{
            EGLContext, EGLDevice, EGLSurface,
            context::{GlAttributes, PixelFormatRequirements},
            display::EGLDisplay,
            ffi, native,
        },
        input::{Axis, AxisSource, ButtonState, KeyState, Keycode},
        renderer::{
            Bind, ImportEgl, Offscreen, RendererSuper,
            gles::{GlesError, GlesRenderbuffer, GlesRenderer},
        },
    },
    reexports::winit::{
        application::ApplicationHandler,
        dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
        event::{
            ElementState, MouseButton as WinitMouseButton, MouseScrollDelta, Touch, TouchPhase,
            WindowEvent,
        },
        event_loop::{ActiveEventLoop, EventLoop},
        platform::{pump_events::EventLoopExtPumpEvents, scancode::PhysicalKeyExtScancode},
        raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle},
        window::{UserAttentionType, Window as WinitWindow, WindowAttributes, WindowId},
    },
    utils::{Buffer as BufferCoords, Clock, DeviceFd, Monotonic, Physical, Rectangle, Size},
};
use tracing::{debug, info_span, warn};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::{Backend, ObjectId},
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_surface},
};
use wayland_egl as wegl;
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1;

pub type SharedPresenterRuntime = Rc<RefCell<PresenterRuntime>>;

#[derive(Debug, Clone, Copy)]
struct RawWaylandDisplay(*mut c_void);

unsafe impl Send for RawWaylandDisplay {}

impl native::EGLNativeDisplay for RawWaylandDisplay {
    fn supported_platforms(&self) -> Vec<native::EGLPlatform<'_>> {
        vec![
            native::EGLPlatform::new(
                ffi::egl::PLATFORM_WAYLAND_KHR,
                "PLATFORM_WAYLAND_KHR",
                self.0,
                vec![ffi::egl::NONE as ffi::EGLint],
                &["EGL_KHR_platform_wayland"],
            ),
            native::EGLPlatform::new(
                ffi::egl::PLATFORM_WAYLAND_EXT,
                "PLATFORM_WAYLAND_EXT",
                self.0,
                vec![ffi::egl::NONE as ffi::EGLint],
                &["EGL_EXT_platform_wayland"],
            ),
        ]
    }

    fn identifier(&self) -> Option<String> {
        Some(format!("wayland:{:p}", self.0))
    }
}

#[derive(Debug, Clone, Copy)]
struct RawXlibDisplay(*mut c_void);

unsafe impl Send for RawXlibDisplay {}

impl native::EGLNativeDisplay for RawXlibDisplay {
    fn supported_platforms(&self) -> Vec<native::EGLPlatform<'_>> {
        vec![
            native::EGLPlatform::new(
                ffi::egl::PLATFORM_X11_KHR,
                "PLATFORM_X11_KHR",
                self.0,
                vec![ffi::egl::NONE as ffi::EGLint],
                &["EGL_KHR_platform_x11"],
            ),
            native::EGLPlatform::new(
                ffi::egl::PLATFORM_X11_EXT,
                "PLATFORM_X11_EXT",
                self.0,
                vec![ffi::egl::NONE as ffi::EGLint],
                &["EGL_EXT_platform_x11"],
            ),
        ]
    }

    fn identifier(&self) -> Option<String> {
        Some(format!("x11:{:p}", self.0))
    }
}

pub struct PresenterRuntime {
    pub backend: Option<PresenterBackend>,
    pub want_window: bool,
    pub pending_events: Vec<PresenterEvent>,
    pub pending_error: Option<String>,
    target: PresenterTarget,
    destroy_flush_pumps: u8,
    gl_attributes: GlAttributes,
    attributes: WindowAttributes,
    offscreen_size: Size<i32, Physical>,
    _span: tracing::Span,
}

impl PresenterRuntime {
    pub fn new() -> Self {
        Self::new_for_target(PresenterTarget::Window)
    }

    pub fn new_offscreen(size: Size<i32, Physical>) -> Self {
        let mut runtime = Self::new_for_target(PresenterTarget::Offscreen);
        runtime.offscreen_size = size;
        runtime
    }

    fn new_for_target(target: PresenterTarget) -> Self {
        let span = info_span!("presentation_winit", window = tracing::field::Empty);
        Self {
            backend: None,
            want_window: true,
            pending_events: Vec::new(),
            pending_error: None,
            target,
            destroy_flush_pumps: 0,
            gl_attributes: GlAttributes {
                version: (3, 0),
                profile: None,
                debug: cfg!(debug_assertions),
                vsync: false,
            },
            attributes: WindowAttributes::default()
                .with_title("Manifold")
                .with_inner_size(LogicalSize::new(1280.0, 800.0))
                .with_visible(true),
            offscreen_size: (1280, 800).into(),
            _span: span,
        }
    }

    pub fn request_window(&mut self) -> bool {
        self.want_window = true;
        self.backend
            .as_ref()
            .is_some_and(PresenterBackend::has_target)
    }

    pub fn close_window(&mut self) {
        self.want_window = false;
        if let Some(backend) = self.backend.as_mut() {
            debug!(
                window_strong_count = backend.window_strong_count(),
                "closing presentation window"
            );
            backend.destroy_frontend();
            self.destroy_flush_pumps = self.destroy_flush_pumps.max(4);
        }
    }

    fn ensure_window(&mut self, event_loop: &ActiveEventLoop) {
        if !self.want_window
            || self
                .backend
                .as_ref()
                .is_some_and(PresenterBackend::has_target)
        {
            return;
        }

        let result = match self.backend.as_mut() {
            Some(backend) => match self.target {
                PresenterTarget::Window => {
                    backend.create_frontend(event_loop, self.attributes.clone())
                }
                PresenterTarget::Offscreen => backend.create_offscreen_target(self.offscreen_size),
            },
            None => match self.target {
                PresenterTarget::Window => PresenterBackend::new(
                    event_loop,
                    self.attributes.clone(),
                    self.gl_attributes,
                    self._span.clone(),
                )
                .map(|backend| {
                    self.backend = Some(backend);
                }),
                PresenterTarget::Offscreen => PresenterBackend::new_offscreen(
                    self.offscreen_size,
                    self.gl_attributes,
                    self._span.clone(),
                )
                .map(|backend| {
                    self.backend = Some(backend);
                }),
            },
        };

        match result {
            Ok(()) => {
                if let Some(window) = self.backend.as_ref().and_then(PresenterBackend::window) {
                    self._span.record("window", Into::<u64>::into(window.id()));
                }
                self.pending_events.push(PresenterEvent::Created);
            }
            Err(err) => {
                self.pending_error = Some(err.to_string());
                self.want_window = false;
                self.pending_events
                    .push(PresenterEvent::CreationFailed(err.to_string()));
            }
        }
    }

    pub fn ensure_offscreen(&mut self) {
        if self.target != PresenterTarget::Offscreen {
            return;
        }
        self.ensure_windowless_target();
    }

    fn ensure_windowless_target(&mut self) {
        if !self.want_window
            || self
                .backend
                .as_ref()
                .is_some_and(PresenterBackend::has_target)
        {
            return;
        }

        let result = match self.backend.as_mut() {
            Some(backend) => backend.create_offscreen_target(self.offscreen_size),
            None => PresenterBackend::new_offscreen(
                self.offscreen_size,
                self.gl_attributes,
                self._span.clone(),
            )
            .map(|backend| {
                self.backend = Some(backend);
            }),
        };

        match result {
            Ok(()) => self.pending_events.push(PresenterEvent::Created),
            Err(err) => {
                self.pending_error = Some(err.to_string());
                self.want_window = false;
                self.pending_events
                    .push(PresenterEvent::CreationFailed(err.to_string()));
            }
        }
    }

    fn active_window_id(&self) -> Option<WindowId> {
        self.backend.as_ref().and_then(PresenterBackend::window_id)
    }

    fn active_window_metrics(&self) -> Option<(PhysicalSize<u32>, f64)> {
        self.backend
            .as_ref()
            .and_then(PresenterBackend::window_metrics)
    }

    fn active_window_scale_factor(&self) -> Option<f64> {
        self.backend
            .as_ref()
            .and_then(PresenterBackend::window_scale_factor)
    }

    fn active_is_x11(&self) -> bool {
        self.backend
            .as_ref()
            .is_some_and(PresenterBackend::window_is_x11)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresenterTarget {
    Window,
    Offscreen,
}

#[derive(Debug)]
pub struct PresenterBackend {
    renderer: GlesRenderer,
    _display: EGLDisplay,
    target: Option<PresenterRenderTarget>,
    damage_tracking: bool,
    bind_size: Option<Size<i32, Physical>>,
    gl_attributes: GlAttributes,
    redraw_requested: Cell<bool>,
    _span: tracing::Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaylandEglBind {
    RendererDisplay,
    Unsupported,
}

#[derive(Debug)]
struct PresenterFrontend {
    egl_surface: EGLSurface,
    window: Arc<WinitWindow>,
    is_x11: bool,
}

#[derive(Debug)]
struct PresenterOffscreenTarget {
    renderbuffer: GlesRenderbuffer,
    size: Size<i32, Physical>,
}

#[derive(Debug)]
enum PresenterRenderTarget {
    Window(PresenterFrontend),
    Offscreen(PresenterOffscreenTarget),
}

impl PresenterBackend {
    fn new(
        event_loop: &ActiveEventLoop,
        attributes: WindowAttributes,
        gl_attributes: GlAttributes,
        span: tracing::Span,
    ) -> Result<Self, PresenterBackendError> {
        let span_for_guard = span.clone();
        let _guard = span_for_guard.enter();
        let window = Arc::new(event_loop.create_window(attributes)?);
        debug!("presentation window created");

        Self::from_window(window, gl_attributes, span)
    }

    fn from_window(
        window: Arc<WinitWindow>,
        gl_attributes: GlAttributes,
        span: tracing::Span,
    ) -> Result<Self, PresenterBackendError> {
        let span_for_guard = span.clone();
        let _guard = span_for_guard.enter();
        let display = display_for_window(&window)?;
        let context = create_egl_context(&display, gl_attributes)?;
        let (egl_surface, is_x11) = create_egl_surface(&window, &display, &context)?;

        let _ = context.unbind();
        let renderer = unsafe { GlesRenderer::new(context)? };
        let damage_tracking = display.supports_damage();
        drop(_guard);

        Ok(Self {
            renderer,
            _display: display,
            target: Some(PresenterRenderTarget::Window(PresenterFrontend {
                egl_surface,
                window,
                is_x11,
            })),
            damage_tracking,
            bind_size: None,
            gl_attributes,
            redraw_requested: Cell::new(false),
            _span: span,
        })
    }

    pub fn new_offscreen(
        size: Size<i32, Physical>,
        gl_attributes: GlAttributes,
        span: tracing::Span,
    ) -> Result<Self, PresenterBackendError> {
        let span_for_guard = span.clone();
        let _guard = span_for_guard.enter();
        let display = offscreen_egl_display()?;
        let context = if env::var_os("MANIFOLD_ALLOW_SOFTWARE_EGL_CONFIG").is_some() {
            EGLContext::new(&display)?
        } else {
            create_egl_context(&display, gl_attributes)?
        };
        let _ = context.unbind();
        let mut renderer = unsafe { GlesRenderer::new(context)? };
        let renderbuffer = create_offscreen_renderbuffer(&mut renderer, size)?;
        drop(_guard);

        Ok(Self {
            renderer,
            _display: display,
            target: Some(PresenterRenderTarget::Offscreen(PresenterOffscreenTarget {
                renderbuffer,
                size,
            })),
            damage_tracking: false,
            bind_size: None,
            gl_attributes,
            redraw_requested: Cell::new(true),
            _span: span,
        })
    }

    fn create_frontend(
        &mut self,
        event_loop: &ActiveEventLoop,
        attributes: WindowAttributes,
    ) -> Result<(), PresenterBackendError> {
        let span_for_guard = self._span.clone();
        let _guard = span_for_guard.enter();
        let window = Arc::new(event_loop.create_window(attributes)?);
        debug!("presentation window recreated");
        match create_egl_surface(
            &window,
            self.renderer.egl_context().display(),
            self.renderer.egl_context(),
        ) {
            Ok((egl_surface, is_x11)) => {
                self.target = Some(PresenterRenderTarget::Window(PresenterFrontend {
                    egl_surface,
                    window,
                    is_x11,
                }));
                self.bind_size = None;
                Ok(())
            }
            Err(err) => {
                warn!(
                    ?err,
                    "failed to recreate presentation EGL surface with existing display; rebuilding presenter backend"
                );
                if let Err(unbind_err) = self.renderer.egl_context().unbind() {
                    warn!(
                        ?unbind_err,
                        "failed to unbind EGL context before rebuilding presenter backend"
                    );
                }
                let rebuilt = Self::from_window(window, self.gl_attributes, self._span.clone())?;
                *self = rebuilt;
                Ok(())
            }
        }
    }

    fn create_offscreen_target(
        &mut self,
        size: Size<i32, Physical>,
    ) -> Result<(), PresenterBackendError> {
        let renderbuffer = create_offscreen_renderbuffer(&mut self.renderer, size)?;
        self.target = Some(PresenterRenderTarget::Offscreen(PresenterOffscreenTarget {
            renderbuffer,
            size,
        }));
        self.bind_size = None;
        self.redraw_requested.set(true);
        Ok(())
    }

    pub fn bind_wl_display(
        &mut self,
        display_handle: &DisplayHandle,
    ) -> Result<WaylandEglBind, smithay::backend::egl::Error> {
        match self.renderer.bind_wl_display(display_handle) {
            Ok(()) => Ok(WaylandEglBind::RendererDisplay),
            Err(smithay::backend::egl::Error::EglExtensionNotSupported(extensions)) => {
                debug!(
                    ?extensions,
                    "presentation renderer EGL display cannot bind wl_display; continuing without legacy wl_drm"
                );
                Ok(WaylandEglBind::Unsupported)
            }
            Err(err) => Err(err),
        }
    }
}

fn display_for_window(window: &Arc<WinitWindow>) -> Result<EGLDisplay, PresenterBackendError> {
    match window.display_handle().map(|handle| handle.as_raw()) {
        Ok(RawDisplayHandle::Wayland(handle)) => unsafe {
            Ok(EGLDisplay::new(RawWaylandDisplay(
                handle.display.as_ptr().cast(),
            ))?)
        },
        Ok(RawDisplayHandle::Xlib(handle)) => {
            let display = handle
                .display
                .ok_or(PresenterBackendError::MissingXlibDisplay)?;
            unsafe { Ok(EGLDisplay::new(RawXlibDisplay(display.as_ptr().cast()))?) }
        }
        _ => unsafe { Ok(EGLDisplay::new(window.clone())?) },
    }
}

fn create_egl_context(
    display: &EGLDisplay,
    gl_attributes: GlAttributes,
) -> Result<EGLContext, PresenterBackendError> {
    let mut ten_bit = PixelFormatRequirements::_10_bit();
    let mut eight_bit = PixelFormatRequirements::_8_bit();
    if env::var_os("MANIFOLD_ALLOW_SOFTWARE_EGL_CONFIG").is_some() {
        ten_bit.hardware_accelerated = None;
        eight_bit.hardware_accelerated = None;
    }

    Ok(EGLContext::new_with_config(display, gl_attributes, ten_bit)
        .or_else(|_| EGLContext::new_with_config(display, gl_attributes, eight_bit))?)
}

impl PresenterBackend {
    fn destroy_frontend(&mut self) {
        if let Err(err) = self.renderer.egl_context().unbind() {
            warn!(
                ?err,
                "failed to unbind EGL context before closing presentation window"
            );
        }
        self.target = None;
        self.bind_size = None;
    }

    const fn has_target(&self) -> bool {
        self.target.is_some()
    }

    fn window(&self) -> Option<&WinitWindow> {
        match self.target.as_ref() {
            Some(PresenterRenderTarget::Window(frontend)) => Some(&frontend.window),
            _ => None,
        }
    }

    fn window_id(&self) -> Option<WindowId> {
        self.window().map(WinitWindow::id)
    }

    fn window_scale_factor(&self) -> Option<f64> {
        self.window()
            .map(|window| normalize_host_scale_factor(window.scale_factor()))
    }

    fn window_metrics(&self) -> Option<(PhysicalSize<u32>, f64)> {
        self.window().map(|window| {
            (
                window.inner_size(),
                normalize_host_scale_factor(window.scale_factor()),
            )
        })
    }

    const fn window_is_x11(&self) -> bool {
        matches!(
            self.target.as_ref(),
            Some(PresenterRenderTarget::Window(PresenterFrontend {
                is_x11: true,
                ..
            }))
        )
    }

    fn window_strong_count(&self) -> usize {
        match self.target.as_ref() {
            Some(PresenterRenderTarget::Window(frontend)) => Arc::strong_count(&frontend.window),
            _ => 0,
        }
    }

    pub fn window_size(&self) -> Size<i32, Physical> {
        match self
            .target
            .as_ref()
            .expect("presentation size requested without render target")
        {
            PresenterRenderTarget::Window(frontend) => {
                let (w, h): (i32, i32) = frontend.window.inner_size().into();
                (w, h).into()
            }
            PresenterRenderTarget::Offscreen(target) => target.size,
        }
    }

    pub fn scale_factor(&self) -> f64 {
        match self.target.as_ref() {
            Some(PresenterRenderTarget::Window(frontend)) => {
                normalize_host_scale_factor(frontend.window.scale_factor())
            }
            Some(PresenterRenderTarget::Offscreen(_)) | None => 1.0,
        }
    }

    pub fn request_redraw(&self) {
        match self.target.as_ref() {
            Some(PresenterRenderTarget::Window(frontend)) => frontend.window.request_redraw(),
            Some(PresenterRenderTarget::Offscreen(_)) => self.redraw_requested.set(true),
            None => {}
        }
    }

    pub fn focus_window(&self, activation_token: Option<&str>) -> bool {
        let Some(window) = self.window() else {
            return false;
        };
        window.set_visible(true);
        if let Some(activation_token) = activation_token.filter(|token| !token.is_empty())
            && let Err(err) = activate_wayland_window_with_token(window, activation_token)
        {
            debug!(
                ?err,
                "failed to activate presentation window with notification activation token"
            );
        }
        window.request_user_attention(Some(UserAttentionType::Critical));
        window.focus_window();
        true
    }

    pub const fn take_redraw_requested(&self) -> bool {
        self.redraw_requested.replace(false)
    }

    pub const fn target_name(&self) -> &'static str {
        match self.target.as_ref() {
            Some(PresenterRenderTarget::Window(_)) => "winit",
            Some(PresenterRenderTarget::Offscreen(_)) => "offscreen",
            None => "none",
        }
    }

    pub const fn renderer(&mut self) -> &mut GlesRenderer {
        &mut self.renderer
    }

    pub fn reset_target_after_context_loss(&mut self) {
        if let Err(err) = self.renderer.egl_context().unbind() {
            warn!(
                ?err,
                "failed to unbind EGL context before resetting presentation target"
            );
        }
        self.target = None;
        self.bind_size = None;
        self.redraw_requested.set(true);
    }

    pub fn bind(
        &mut self,
    ) -> Result<
        (
            &mut GlesRenderer,
            <GlesRenderer as RendererSuper>::Framebuffer<'_>,
        ),
        SwapBuffersError,
    > {
        match self
            .target
            .as_mut()
            .expect("presentation render requested without render target")
        {
            PresenterRenderTarget::Window(frontend) => {
                let (w, h): (i32, i32) = frontend.window.inner_size().into();
                let window_size = Size::from((w, h));
                if Some(window_size) != self.bind_size {
                    frontend
                        .egl_surface
                        .resize(window_size.w, window_size.h, 0, 0);
                }
                self.bind_size = Some(window_size);
                let fb = self.renderer.bind(&mut frontend.egl_surface)?;
                Ok((&mut self.renderer, fb))
            }
            PresenterRenderTarget::Offscreen(target) => {
                self.bind_size = Some(target.size);
                let fb = self.renderer.bind(&mut target.renderbuffer)?;
                Ok((&mut self.renderer, fb))
            }
        }
    }

    pub fn submit(
        &mut self,
        damage: Option<&[Rectangle<i32, Physical>]>,
    ) -> Result<(), SwapBuffersError> {
        let mut damage = match damage {
            Some(damage) if self.damage_tracking && !damage.is_empty() => {
                let bind_size = self
                    .bind_size
                    .expect("submitting without ever binding the renderer");
                Some(
                    damage
                        .iter()
                        .map(|rect| {
                            Rectangle::new(
                                (rect.loc.x, bind_size.h - rect.loc.y - rect.size.h).into(),
                                rect.size,
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            }
            _ => None,
        };
        if let Some(PresenterRenderTarget::Window(frontend)) = self.target.as_mut() {
            frontend.window.pre_present_notify();
            frontend.egl_surface.swap_buffers(damage.as_deref_mut())?;
        }
        Ok(())
    }
}

struct ActivationProxyState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ActivationProxyState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_activation_v1::XdgActivationV1, ()> for ActivationProxyState {
    fn event(
        _state: &mut Self,
        _proxy: &xdg_activation_v1::XdgActivationV1,
        _event: xdg_activation_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

fn activate_wayland_window_with_token(
    window: &WinitWindow,
    activation_token: &str,
) -> AnyResult<bool> {
    let display_ptr = match window
        .display_handle()
        .context("get window display handle")?
        .as_raw()
    {
        RawDisplayHandle::Wayland(handle) => handle.display.as_ptr().cast(),
        _ => return Ok(false),
    };
    let surface_ptr = match window
        .window_handle()
        .context("get window handle")?
        .as_raw()
    {
        RawWindowHandle::Wayland(handle) => handle.surface.as_ptr().cast(),
        _ => return Ok(false),
    };

    let backend = unsafe { Backend::from_foreign_display(display_ptr) };
    let connection = Connection::from_backend(backend);
    let (globals, event_queue) =
        registry_queue_init::<ActivationProxyState>(&connection).context("read Wayland globals")?;
    let activation = globals
        .bind::<xdg_activation_v1::XdgActivationV1, _, _>(&event_queue.handle(), 1..=1, ())
        .context("bind xdg_activation_v1")?;
    let surface_id = unsafe {
        ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface_ptr)
            .context("wrap presentation wl_surface")?
    };
    let surface =
        wl_surface::WlSurface::from_id(&connection, surface_id).context("proxy wl_surface")?;

    activation.activate(activation_token.to_string(), &surface);
    connection
        .flush()
        .context("flush Wayland activation request")?;
    Ok(true)
}

fn create_offscreen_renderbuffer(
    renderer: &mut GlesRenderer,
    size: Size<i32, Physical>,
) -> Result<GlesRenderbuffer, GlesError> {
    let buffer_size: Size<i32, BufferCoords> = (size.w, size.h).into();
    renderer.create_buffer(Fourcc::Abgr8888, buffer_size)
}

fn offscreen_egl_display() -> Result<EGLDisplay, PresenterBackendError> {
    let mut failures = Vec::new();
    for path in render_node_candidates() {
        match gbm_egl_display_from_path(&path) {
            Ok(display) => return Ok(display),
            Err(err) => failures.push(format!("{}:{err}", path.display())),
        }
    }

    match unsafe { EGLDisplay::new(native::EGLSurfacelessDisplay) } {
        Ok(display) => Ok(display),
        Err(err) => {
            if failures.is_empty() {
                Err(err.into())
            } else {
                Err(PresenterBackendError::OffscreenRenderer(format!(
                    "gbm render-node attempts failed [{}]; surfaceless fallback failed: {err}",
                    failures.join(", ")
                )))
            }
        }
    }
}

fn gbm_egl_display_from_path(path: &PathBuf) -> Result<EGLDisplay, PresenterBackendError> {
    let file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let owned_fd: OwnedFd = file.into();
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(owned_fd));
    let gbm = GbmDevice::new(drm_fd)?;
    Ok(unsafe { EGLDisplay::new(gbm)? })
}

fn render_node_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("MANIFOLD_RENDER_NODE") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(devices) = EGLDevice::enumerate() {
        for device in devices {
            if let Ok(path) = device.render_device_path() {
                candidates.push(path);
            }
        }
    }
    if let Ok(entries) = fs::read_dir("/dev/dri") {
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("renderD"))
            })
            .collect::<Vec<_>>();
        paths.sort();
        candidates.extend(paths);
    }
    candidates.dedup();
    candidates
}

fn create_egl_surface(
    window: &Arc<WinitWindow>,
    display: &EGLDisplay,
    context: &EGLContext,
) -> Result<(EGLSurface, bool), PresenterBackendError> {
    match window.window_handle().map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Wayland(handle)) => {
            let size = window.inner_size();
            let surface = unsafe {
                wegl::WlEglSurface::new_from_raw(
                    handle.surface.as_ptr().cast(),
                    size.width as i32,
                    size.height as i32,
                )
            }
            .map_err(|err| PresenterBackendError::Surface(err.to_string()))?;
            unsafe {
                Ok((
                    EGLSurface::new(
                        display,
                        context
                            .pixel_format()
                            .expect("EGL context has pixel format"),
                        context.config_id(),
                        surface,
                    )?,
                    false,
                ))
            }
        }
        Ok(RawWindowHandle::Xlib(handle)) => unsafe {
            Ok((
                EGLSurface::new(
                    display,
                    context
                        .pixel_format()
                        .expect("EGL context has pixel format"),
                    context.config_id(),
                    native::XlibWindow(handle.window),
                )?,
                true,
            ))
        },
        _ => Err(PresenterBackendError::UnsupportedWindowHandle),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PresenterBackendError {
    #[error("create winit window: {0}")]
    Window(#[from] smithay::reexports::winit::error::OsError),
    #[error("create EGL display: {0}")]
    Egl(#[from] smithay::backend::egl::Error),
    #[error("create EGL surface: {0}")]
    EglSurface(#[from] smithay::backend::egl::EGLError),
    #[error("create EGL renderer: {0}")]
    Renderer(#[from] GlesError),
    #[error("create GBM offscreen display: {0}")]
    Gbm(#[from] IoError),
    #[error("create offscreen presentation renderer: {0}")]
    OffscreenRenderer(String),
    #[error("create Wayland EGL surface: {0}")]
    Surface(String),
    #[error("unsupported window handle")]
    UnsupportedWindowHandle,
    #[error("xlib display handle is missing")]
    MissingXlibDisplay,
}

#[derive(Debug)]
pub enum PresenterEvent {
    Created,
    CreationFailed(String),
    Resized {
        size: Size<i32, Physical>,
        scale_factor: f64,
    },
    ScaleFactorChanged {
        scale_factor: f64,
    },
    Redraw,
    CloseRequested,
    Focus {
        focused: bool,
    },
    Keyboard {
        key_code: Keycode,
        state: KeyState,
        time: u32,
    },
    PointerMotion {
        position: smithay::utils::Point<f64, smithay::utils::Logical>,
        time: u32,
    },
    PointerButton {
        button: u32,
        state: ButtonState,
        time: u32,
    },
    PointerAxis {
        horizontal_amount: Option<f64>,
        vertical_amount: Option<f64>,
        horizontal_v120: Option<i32>,
        vertical_v120: Option<i32>,
        source: AxisSource,
        time: u32,
    },
}

pub struct PresenterEventSource {
    runtime: SharedPresenterRuntime,
    event_loop: Generic<EventLoop<()>>,
    fake_token: Option<Token>,
    pending_events: Vec<PresenterEvent>,
    clock: Clock<Monotonic>,
    key_counter: u32,
}

impl PresenterEventSource {
    pub fn new(
        runtime: SharedPresenterRuntime,
    ) -> Result<Self, smithay::reexports::winit::error::EventLoopError> {
        let event_loop = new_presenter_event_loop()?;
        event_loop.set_control_flow(smithay::reexports::winit::event_loop::ControlFlow::Poll);
        let event_loop = Generic::new(event_loop, Interest::READ, calloop::Mode::Level);
        Ok(Self {
            runtime,
            event_loop,
            fake_token: None,
            pending_events: Vec::new(),
            clock: Clock::<Monotonic>::new(),
            key_counter: 0,
        })
    }

    fn dispatch_new_events<F>(
        &mut self,
        callback: F,
    ) -> smithay::reexports::winit::platform::pump_events::PumpStatus
    where
        F: FnMut(PresenterEvent),
    {
        let event_loop = unsafe { self.event_loop.get_mut() };
        let mut callback = callback;
        let mut status = event_loop.pump_app_events(
            Some(Duration::ZERO),
            &mut PresenterApp {
                runtime: Rc::clone(&self.runtime),
                clock: &self.clock,
                key_counter: &mut self.key_counter,
                callback: &mut callback,
            },
        );

        loop {
            if matches!(
                status,
                smithay::reexports::winit::platform::pump_events::PumpStatus::Exit(_)
            ) {
                return status;
            }

            let should_flush_destroy = {
                let mut runtime = self.runtime.borrow_mut();
                if runtime.destroy_flush_pumps == 0 {
                    false
                } else {
                    runtime.destroy_flush_pumps -= 1;
                    true
                }
            };
            if !should_flush_destroy {
                return status;
            }

            status = event_loop.pump_app_events(
                Some(Duration::ZERO),
                &mut PresenterApp {
                    runtime: Rc::clone(&self.runtime),
                    clock: &self.clock,
                    key_counter: &mut self.key_counter,
                    callback: &mut callback,
                },
            );
        }
    }
}

fn new_presenter_event_loop()
-> Result<EventLoop<()>, smithay::reexports::winit::error::EventLoopError> {
    let mut builder = EventLoop::builder();
    #[cfg(all(
        unix,
        not(target_os = "android"),
        not(target_os = "ios"),
        not(target_os = "macos")
    ))]
    {
        smithay::reexports::winit::platform::wayland::EventLoopBuilderExtWayland::with_any_thread(
            &mut builder,
            true,
        );
        smithay::reexports::winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(
            &mut builder,
            true,
        );
    }
    builder.build()
}

struct PresenterApp<'a, F: FnMut(PresenterEvent)> {
    runtime: SharedPresenterRuntime,
    clock: &'a Clock<Monotonic>,
    key_counter: &'a mut u32,
    callback: F,
}

impl<F: FnMut(PresenterEvent)> PresenterApp<'_, F> {
    fn timestamp(&self) -> u32 {
        self.clock.now().as_millis()
    }

    fn emit_pending(&mut self) {
        let pending = self
            .runtime
            .borrow_mut()
            .pending_events
            .drain(..)
            .collect::<Vec<_>>();
        for event in pending {
            (self.callback)(event);
        }
    }
}

impl<F: FnMut(PresenterEvent)> ApplicationHandler for PresenterApp<'_, F> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        {
            self.runtime.borrow_mut().ensure_window(event_loop);
        }
        self.emit_pending();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        {
            self.runtime.borrow_mut().ensure_window(event_loop);
        }
        self.emit_pending();
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let active_window_id = { self.runtime.borrow().active_window_id() };
        if active_window_id != Some(window_id) {
            return;
        }

        match event {
            WindowEvent::Resized(size) => {
                let scale_factor =
                    { self.runtime.borrow().active_window_scale_factor() }.unwrap_or(1.0);
                (self.callback)(PresenterEvent::Resized {
                    size: (size.width as i32, size.height as i32).into(),
                    scale_factor,
                });
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                (self.callback)(PresenterEvent::ScaleFactorChanged {
                    scale_factor: normalize_host_scale_factor(scale_factor),
                });
            }
            WindowEvent::RedrawRequested => (self.callback)(PresenterEvent::Redraw),
            WindowEvent::CloseRequested => (self.callback)(PresenterEvent::CloseRequested),
            WindowEvent::Focused(focused) => (self.callback)(PresenterEvent::Focus { focused }),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } if !is_synthetic && !event.repeat => {
                match event.state {
                    ElementState::Pressed => *self.key_counter += 1,
                    ElementState::Released => {
                        *self.key_counter = self.key_counter.saturating_sub(1);
                    }
                }
                let scancode = event.physical_key.to_scancode().unwrap_or(0);
                let time = self.timestamp();
                (self.callback)(PresenterEvent::Keyboard {
                    key_code: Keycode::new(scancode + 8),
                    state: key_state(event.state),
                    time,
                });
            }
            WindowEvent::CursorMoved { position, .. } => {
                let active_window_metrics = { self.runtime.borrow().active_window_metrics() };
                if let Some((size, scale_factor)) = active_window_metrics {
                    let position = pointer_position(position, size, scale_factor);
                    let time = self.timestamp();
                    (self.callback)(PresenterEvent::PointerMotion { position, time });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scale_factor =
                    { self.runtime.borrow().active_window_scale_factor() }.unwrap_or(1.0);
                let (horizontal_amount, vertical_amount, horizontal_v120, vertical_v120, source) =
                    axis_values(delta, scale_factor);
                let time = self.timestamp();
                (self.callback)(PresenterEvent::PointerAxis {
                    horizontal_amount,
                    vertical_amount,
                    horizontal_v120,
                    vertical_v120,
                    source,
                    time,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let is_x11 = { self.runtime.borrow().active_is_x11() };
                let button = button_code(button, is_x11);
                let state = button_state(state);
                let time = self.timestamp();
                (self.callback)(PresenterEvent::PointerButton {
                    button,
                    state,
                    time,
                });
            }
            WindowEvent::Touch(Touch {
                phase: TouchPhase::Started | TouchPhase::Moved,
                location,
                ..
            }) => {
                let active_window_metrics = { self.runtime.borrow().active_window_metrics() };
                if let Some((size, scale_factor)) = active_window_metrics {
                    let position = pointer_position(location, size, scale_factor);
                    let time = self.timestamp();
                    (self.callback)(PresenterEvent::PointerMotion { position, time });
                }
            }
            _ => {}
        }
    }
}

impl EventSource for PresenterEventSource {
    type Event = PresenterEvent;
    type Metadata = ();
    type Ret = ();
    type Error = IoError;

    const NEEDS_EXTRA_LIFECYCLE_EVENTS: bool = true;

    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>> {
        let mut pending_events = std::mem::take(&mut self.pending_events);
        let callback = |event| pending_events.push(event);
        self.dispatch_new_events(callback);
        self.pending_events = pending_events;
        if self.pending_events.is_empty() {
            Ok(None)
        } else {
            Ok(Some((
                Readiness::EMPTY,
                self.fake_token.expect("registered"),
            )))
        }
    }

    fn process_events<F>(
        &mut self,
        _readiness: Readiness,
        _token: Token,
        mut callback: F,
    ) -> Result<PostAction, Self::Error>
    where
        F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        let mut callback = |event| callback(event, &mut ());
        for event in self.pending_events.drain(..) {
            callback(event);
        }
        Ok(match self.dispatch_new_events(callback) {
            smithay::reexports::winit::platform::pump_events::PumpStatus::Continue => {
                PostAction::Continue
            }
            smithay::reexports::winit::platform::pump_events::PumpStatus::Exit(_) => {
                PostAction::Remove
            }
        })
    }

    fn register(
        &mut self,
        poll: &mut calloop::Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        self.fake_token = Some(token_factory.token());
        self.event_loop.register(poll, token_factory)
    }

    fn reregister(
        &mut self,
        poll: &mut calloop::Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        self.event_loop.reregister(poll, token_factory)
    }

    fn unregister(&mut self, poll: &mut calloop::Poll) -> calloop::Result<()> {
        self.event_loop.unregister(poll)
    }
}

const fn key_state(state: ElementState) -> KeyState {
    match state {
        ElementState::Pressed => KeyState::Pressed,
        ElementState::Released => KeyState::Released,
    }
}

const fn button_state(state: ElementState) -> ButtonState {
    match state {
        ElementState::Pressed => ButtonState::Pressed,
        ElementState::Released => ButtonState::Released,
    }
}

fn button_code(button: WinitMouseButton, is_x11: bool) -> u32 {
    match button {
        WinitMouseButton::Left => 0x110,
        WinitMouseButton::Right => 0x111,
        WinitMouseButton::Middle => 0x112,
        WinitMouseButton::Back => 0x116,
        WinitMouseButton::Forward => 0x115,
        WinitMouseButton::Other(button) => {
            if is_x11 {
                xorg_mouse_to_libinput(u32::from(button))
            } else {
                u32::from(button)
            }
        }
    }
}

const fn xorg_mouse_to_libinput(xorg: u32) -> u32 {
    match xorg {
        0 => 0,
        1 => 0x110,
        2 => 0x112,
        3 => 0x111,
        _ => xorg - 8 + 0x113,
    }
}

pub fn normalize_host_scale_factor(scale_factor: f64) -> f64 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    }
}

fn pointer_position(
    position: PhysicalPosition<f64>,
    size: PhysicalSize<u32>,
    scale_factor: f64,
) -> smithay::utils::Point<f64, smithay::utils::Logical> {
    let scale_factor = normalize_host_scale_factor(scale_factor);
    let width = f64::from(size.width.max(1)) / scale_factor;
    let height = f64::from(size.height.max(1)) / scale_factor;
    smithay::utils::Point::from((
        (position.x / scale_factor).clamp(0.0, width),
        (position.y / scale_factor).clamp(0.0, height),
    ))
}

fn axis_values(
    delta: MouseScrollDelta,
    scale_factor: f64,
) -> (
    Option<f64>,
    Option<f64>,
    Option<i32>,
    Option<i32>,
    AxisSource,
) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (
            None,
            None,
            Some((-x * 120.0) as i32),
            Some((-y * 120.0) as i32),
            AxisSource::Wheel,
        ),
        MouseScrollDelta::PixelDelta(delta) => {
            let scale_factor = normalize_host_scale_factor(scale_factor);
            (
                Some(-delta.x / scale_factor),
                Some(-delta.y / scale_factor),
                None,
                None,
                AxisSource::Continuous,
            )
        }
    }
}

pub fn axis_frame(
    horizontal_amount: Option<f64>,
    vertical_amount: Option<f64>,
    horizontal_v120: Option<i32>,
    vertical_v120: Option<i32>,
    source: AxisSource,
    time: u32,
) -> smithay::input::pointer::AxisFrame {
    let mut frame = smithay::input::pointer::AxisFrame::new(time).source(source);
    if let Some(amount) = horizontal_amount {
        frame = frame.value(Axis::Horizontal, amount);
    }
    if let Some(amount) = vertical_amount {
        frame = frame.value(Axis::Vertical, amount);
    }
    if let Some(v120) = horizontal_v120 {
        frame = frame.v120(Axis::Horizontal, v120);
    }
    if let Some(v120) = vertical_v120 {
        frame = frame.v120(Axis::Vertical, v120);
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::{axis_values, normalize_host_scale_factor, pointer_position};
    use smithay::{
        backend::input::AxisSource,
        reexports::winit::{
            dpi::{PhysicalPosition, PhysicalSize},
            event::MouseScrollDelta,
        },
    };

    fn assert_close(left: f64, right: f64) {
        assert!((left - right).abs() <= f64::EPSILON);
    }

    #[test]
    fn normalizes_invalid_host_scale_factors_to_one() {
        assert_close(normalize_host_scale_factor(2.0), 2.0);
        assert_close(normalize_host_scale_factor(0.0), 1.0);
        assert_close(normalize_host_scale_factor(f64::NAN), 1.0);
    }

    #[test]
    fn pointer_position_uses_logical_coordinates_for_host_scale() {
        let position = pointer_position(
            PhysicalPosition::new(640.0, 400.0),
            PhysicalSize::new(1280, 800),
            2.0,
        );
        assert_close(position.x, 320.0);
        assert_close(position.y, 200.0);
    }

    #[test]
    fn pointer_position_clamps_after_host_scale_conversion() {
        let position = pointer_position(
            PhysicalPosition::new(1400.0, 900.0),
            PhysicalSize::new(1280, 800),
            2.0,
        );
        assert_close(position.x, 640.0);
        assert_close(position.y, 400.0);
    }

    #[test]
    fn pixel_scroll_delta_uses_logical_coordinates_for_host_scale() {
        let (horizontal, vertical, horizontal_v120, vertical_v120, source) = axis_values(
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(12.0, -8.0)),
            2.0,
        );
        assert_close(horizontal.expect("horizontal pixel delta"), -6.0);
        assert_close(vertical.expect("vertical pixel delta"), 4.0);
        assert!(horizontal_v120.is_none());
        assert!(vertical_v120.is_none());
        assert_eq!(source, AxisSource::Continuous);
    }
}
