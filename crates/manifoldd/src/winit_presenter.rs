use std::{cell::RefCell, ffi::c_void, io::Error as IoError, rc::Rc, sync::Arc, time::Duration};

use calloop::{
    EventSource, Interest, PostAction, Readiness, Token, TokenFactory, generic::Generic,
};
use smithay::{
    backend::{
        SwapBuffersError,
        egl::{
            EGLContext, EGLSurface,
            context::{GlAttributes, PixelFormatRequirements},
            display::EGLDisplay,
            ffi, native,
        },
        input::{Axis, AxisSource, ButtonState, KeyState, Keycode},
        renderer::{
            Bind, RendererSuper,
            gles::{GlesError, GlesRenderer},
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
        window::{Window as WinitWindow, WindowAttributes, WindowId},
    },
    utils::{Clock, Monotonic, Physical, Rectangle, Size},
};
use tracing::{debug, info_span};
use wayland_egl as wegl;

pub(crate) type SharedPresenterRuntime = Rc<RefCell<PresenterRuntime>>;

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

pub(crate) struct PresenterRuntime {
    pub backend: Option<PresenterBackend>,
    pub want_window: bool,
    pub pending_events: Vec<PresenterEvent>,
    pub pending_error: Option<String>,
    destroy_flush_pumps: u8,
    gl_attributes: GlAttributes,
    attributes: WindowAttributes,
    _span: tracing::Span,
}

impl PresenterRuntime {
    pub fn new() -> Self {
        let span = info_span!("presentation_winit", window = tracing::field::Empty);
        Self {
            backend: None,
            want_window: true,
            pending_events: Vec::new(),
            pending_error: None,
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
            _span: span,
        }
    }

    pub fn request_window(&mut self) {
        self.want_window = true;
    }

    pub fn destroy_window(&mut self) {
        self.want_window = false;
        if let Some(backend) = self.backend.take() {
            debug!(
                window_strong_count = Arc::strong_count(&backend.window),
                "destroying presentation window"
            );
            drop(backend);
            self.destroy_flush_pumps = self.destroy_flush_pumps.max(4);
        }
    }

    fn ensure_window(&mut self, event_loop: &ActiveEventLoop) {
        if !self.want_window || self.backend.is_some() {
            return;
        }
        match PresenterBackend::new(
            event_loop,
            self.attributes.clone(),
            self.gl_attributes,
            self._span.clone(),
        ) {
            Ok(backend) => {
                self._span
                    .record("window", Into::<u64>::into(backend.window().id()));
                self.pending_events.push(PresenterEvent::Created);
                self.backend = Some(backend);
            }
            Err(err) => {
                self.pending_error = Some(err.to_string());
                self.want_window = false;
                self.pending_events
                    .push(PresenterEvent::CreationFailed(err.to_string()));
            }
        }
    }

    fn active_window_id(&self) -> Option<WindowId> {
        self.backend.as_ref().map(|backend| backend.window().id())
    }

    fn active_window_size(&self) -> Option<PhysicalSize<u32>> {
        self.backend
            .as_ref()
            .map(|backend| backend.window().inner_size())
    }

    fn active_is_x11(&self) -> bool {
        self.backend.as_ref().is_some_and(|backend| backend.is_x11)
    }
}

#[derive(Debug)]
pub(crate) struct PresenterBackend {
    renderer: GlesRenderer,
    _display: EGLDisplay,
    egl_surface: EGLSurface,
    window: Arc<WinitWindow>,
    damage_tracking: bool,
    bind_size: Option<Size<i32, Physical>>,
    _span: tracing::Span,
    is_x11: bool,
}

impl PresenterBackend {
    fn new(
        event_loop: &ActiveEventLoop,
        attributes: WindowAttributes,
        gl_attributes: GlAttributes,
        span: tracing::Span,
    ) -> Result<Self, PresenterBackendError> {
        let _guard = span.enter();
        let window = Arc::new(event_loop.create_window(attributes)?);
        debug!("presentation window created");

        let display = match window.display_handle().map(|handle| handle.as_raw()) {
            Ok(RawDisplayHandle::Wayland(handle)) => unsafe {
                EGLDisplay::new(RawWaylandDisplay(handle.display.as_ptr().cast()))?
            },
            Ok(RawDisplayHandle::Xlib(handle)) => {
                let display = handle
                    .display
                    .ok_or(PresenterBackendError::MissingXlibDisplay)?;
                unsafe { EGLDisplay::new(RawXlibDisplay(display.as_ptr().cast()))? }
            }
            _ => unsafe { EGLDisplay::new(window.clone())? },
        };
        let context = EGLContext::new_with_config(
            &display,
            gl_attributes,
            PixelFormatRequirements::_10_bit(),
        )
        .or_else(|_| {
            EGLContext::new_with_config(&display, gl_attributes, PixelFormatRequirements::_8_bit())
        })?;

        let (egl_surface, is_x11) = match window.window_handle().map(|handle| handle.as_raw()) {
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
                    (
                        EGLSurface::new(
                            &display,
                            context
                                .pixel_format()
                                .expect("EGL context has pixel format"),
                            context.config_id(),
                            surface,
                        )?,
                        false,
                    )
                }
            }
            Ok(RawWindowHandle::Xlib(handle)) => unsafe {
                (
                    EGLSurface::new(
                        &display,
                        context
                            .pixel_format()
                            .expect("EGL context has pixel format"),
                        context.config_id(),
                        native::XlibWindow(handle.window),
                    )?,
                    true,
                )
            },
            _ => return Err(PresenterBackendError::UnsupportedWindowHandle),
        };

        let _ = context.unbind();
        let renderer = unsafe { GlesRenderer::new(context)? };
        let damage_tracking = display.supports_damage();
        drop(_guard);

        Ok(Self {
            renderer,
            _display: display,
            egl_surface,
            window,
            damage_tracking,
            bind_size: None,
            _span: span,
            is_x11,
        })
    }

    pub fn window_size(&self) -> Size<i32, Physical> {
        let (w, h): (i32, i32) = self.window.inner_size().into();
        (w, h).into()
    }

    pub fn window(&self) -> &WinitWindow {
        &self.window
    }

    pub fn renderer(&mut self) -> &mut GlesRenderer {
        &mut self.renderer
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
        let window_size = self.window_size();
        if Some(window_size) != self.bind_size {
            self.egl_surface.resize(window_size.w, window_size.h, 0, 0);
        }
        self.bind_size = Some(window_size);
        let fb = self.renderer.bind(&mut self.egl_surface)?;
        Ok((&mut self.renderer, fb))
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
        self.window.pre_present_notify();
        self.egl_surface.swap_buffers(damage.as_deref_mut())?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PresenterBackendError {
    #[error("create winit window: {0}")]
    Window(#[from] smithay::reexports::winit::error::OsError),
    #[error("create EGL display: {0}")]
    Egl(#[from] smithay::backend::egl::Error),
    #[error("create EGL surface: {0}")]
    EglSurface(#[from] smithay::backend::egl::EGLError),
    #[error("create EGL renderer: {0}")]
    Renderer(#[from] GlesError),
    #[error("create Wayland EGL surface: {0}")]
    Surface(String),
    #[error("unsupported window handle")]
    UnsupportedWindowHandle,
    #[error("xlib display handle is missing")]
    MissingXlibDisplay,
}

#[derive(Debug)]
pub(crate) enum PresenterEvent {
    Created,
    CreationFailed(String),
    Resized {
        size: Size<i32, Physical>,
    },
    Redraw,
    CloseRequested,
    Focus,
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

pub(crate) struct PresenterEventSource {
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
        let event_loop = EventLoop::builder().build()?;
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

struct PresenterApp<'a, F: FnMut(PresenterEvent)> {
    runtime: SharedPresenterRuntime,
    clock: &'a Clock<Monotonic>,
    key_counter: &'a mut u32,
    callback: F,
}

impl<F: FnMut(PresenterEvent)> PresenterApp<'_, F> {
    fn timestamp(&self) -> u32 {
        self.clock.now().as_millis() as u32
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
        self.runtime.borrow_mut().ensure_window(event_loop);
        self.emit_pending();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.runtime.borrow_mut().ensure_window(event_loop);
        self.emit_pending();
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.runtime.borrow().active_window_id() != Some(window_id) {
            return;
        }

        match event {
            WindowEvent::Resized(size) => {
                (self.callback)(PresenterEvent::Resized {
                    size: (size.width as i32, size.height as i32).into(),
                });
            }
            WindowEvent::RedrawRequested => (self.callback)(PresenterEvent::Redraw),
            WindowEvent::CloseRequested => (self.callback)(PresenterEvent::CloseRequested),
            WindowEvent::Focused(_) => (self.callback)(PresenterEvent::Focus),
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
                if let Some(size) = self.runtime.borrow().active_window_size() {
                    let position = pointer_position(position, size);
                    let time = self.timestamp();
                    (self.callback)(PresenterEvent::PointerMotion { position, time });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (horizontal_amount, vertical_amount, horizontal_v120, vertical_v120, source) =
                    axis_values(delta);
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
                let is_x11 = self.runtime.borrow().active_is_x11();
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
                if let Some(size) = self.runtime.borrow().active_window_size() {
                    let position = pointer_position(location, size);
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

fn key_state(state: ElementState) -> KeyState {
    match state {
        ElementState::Pressed => KeyState::Pressed,
        ElementState::Released => KeyState::Released,
    }
}

fn button_state(state: ElementState) -> ButtonState {
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

fn xorg_mouse_to_libinput(xorg: u32) -> u32 {
    match xorg {
        0 => 0,
        1 => 0x110,
        2 => 0x112,
        3 => 0x111,
        _ => xorg - 8 + 0x113,
    }
}

fn pointer_position(
    position: PhysicalPosition<f64>,
    size: PhysicalSize<u32>,
) -> smithay::utils::Point<f64, smithay::utils::Logical> {
    let width = f64::from(size.width.max(1));
    let height = f64::from(size.height.max(1));
    smithay::utils::Point::from((position.x.clamp(0.0, width), position.y.clamp(0.0, height)))
}

fn axis_values(
    delta: MouseScrollDelta,
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
        MouseScrollDelta::PixelDelta(delta) => (
            Some(-delta.x),
            Some(-delta.y),
            None,
            None,
            AxisSource::Continuous,
        ),
    }
}

pub(crate) fn axis_frame(
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
