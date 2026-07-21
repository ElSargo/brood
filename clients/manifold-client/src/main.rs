#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "bounded window, text, and graphics coordinates intentionally narrow to the f32 and integer formats used by WGPU"
)]

use std::{
    collections::{HashMap, HashSet},
    os::fd::AsFd,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use manifold_cli::{
    ClientArgs,
    control::{
        ControlTarget, DmabufFrame, ServerLaunch, connect_or_spawn, dmabuf_resource,
        open_presentation_fast_path, resolve_control_target,
    },
};
use manifold_dmabuf::{SinglePlaneDmabuf, import_single_plane};
use manifold_layout::{LayoutAxis, LayoutRect, LayoutSize, LayoutState};
use wgpu::{Color, CurrentSurfaceTexture};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, ModifiersState, PhysicalKey},
    platform::scancode::PhysicalKeyExtScancode,
    window::{Window, WindowId},
};

mod actor_runtime;
mod plugin_gpu;
mod plugin_graphics;

use actor_runtime::PresentationWasmRuntime;
use manifold_wasm::PresentationEvent;
use plugin_gpu::{GpuCanvasRenderer, GpuCanvasScene};
use plugin_graphics::PluginGraphicsBridge;

fn main() -> Result<()> {
    let args = ClientArgs::parse();
    if args.check_wasm_actors {
        let mut runtime = PresentationWasmRuntime::start(&args.wasm_actors, None)?;
        runtime
            .shutdown()
            .context("stop checked presentation WASM runtime")?;
        drop(runtime);
        eprintln!("presentation WASM actor check passed");
        return Ok(());
    }
    let event_loop = EventLoop::<ClientEvent>::with_user_event()
        .build()
        .context("create event loop")?;
    let event_proxy = event_loop.create_proxy();
    let plugin_graphics = PluginGraphicsBridge::new(event_proxy.clone());
    let (input_sender, fast_path_enabled, backend_connected) = if args.no_server {
        eprintln!("starting Manifold presentation without a backend");
        (None, false, false)
    } else {
        let target = resolve_control_target(args.dbus_name, args.dbus_address);
        let mut server = ServerLaunch::discover()?;
        if let Some(executable) = args.server {
            server.executable = executable;
        }
        let handshake = connect_or_spawn(&target, &server)?;
        eprintln!(
            "connected to Manifold server pid={} protocol={}",
            handshake.server_pid, handshake.protocol_version
        );

        let fast_path_enabled = std::env::var_os("MANIFOLD_DMABUF_HAPPY_PATH").is_some();
        let input_sender = if fast_path_enabled {
            let fast_path = open_presentation_fast_path(&target)?;
            let input = manifold_ipc::CriticalProducer::map(
                fast_path.input_fd.as_fd(),
                fast_path.generation,
            )?;
            let frames = manifold_ipc::CriticalConsumer::map(
                fast_path.frame_fd.as_fd(),
                fast_path.generation,
            )?;
            let frame_notifications =
                manifold_ipc::EventNotifier::from_owned_fd(fast_path.frame_notify_fd);
            spawn_frame_updates(
                target,
                frames,
                frame_notifications,
                event_loop.create_proxy(),
            )?;
            Some(InputLane {
                producer: input,
                notifications: manifold_ipc::EventNotifier::from_owned_fd(
                    fast_path.input_notify_fd,
                ),
                generation: fast_path.generation,
                sequence: 0,
            })
        } else {
            None
        };
        (input_sender, fast_path_enabled, true)
    };
    let mut application = Application {
        wasm_runtime: Some(PresentationWasmRuntime::start(
            &args.wasm_actors,
            Some(&plugin_graphics),
        )?),
        plugin_graphics: Some(plugin_graphics),
        input_sender,
        fast_path_enabled,
        backend_connected,
        event_proxy: Some(event_proxy),
        ..Application::default()
    };

    let run_result = event_loop
        .run_app(&mut application)
        .context("run event loop");

    if let Some(sender) = application.input_sender.as_mut() {
        let _ = send_input(
            sender,
            manifold_ipc::INPUT_CLIENT_DISCONNECT,
            [0, 0, 0, 0, 0],
        );
    }
    let shutdown_result = application
        .wasm_runtime
        .as_mut()
        .map_or(Ok(()), PresentationWasmRuntime::shutdown)
        .context("stop presentation WASM runtime");
    let error = application.error.take();
    drop(application);
    run_result?;
    shutdown_result?;
    if let Some(error) = error {
        return Err(error);
    }

    Ok(())
}

#[derive(Debug)]
enum ClientEvent {
    SceneFrame {
        sequence: u64,
        width: u32,
        height: u32,
        windows: Vec<SceneWindowUpdate>,
    },
    TransportError(String),
    PluginGraphicsChanged {
        pane: u64,
        sequence: u64,
    },
    RendererFailed {
        generation: u64,
        message: String,
    },
}

#[derive(Debug)]
struct SceneWindowUpdate {
    window_id: u64,
    resource_id: u64,
    rect: [i32; 4],
    active: bool,
    resource: Option<DmabufFrame>,
}

struct PendingScene {
    sequence: u64,
    expected_windows: usize,
    width: u32,
    height: u32,
    windows: Vec<SceneWindowUpdate>,
}

fn spawn_frame_updates(
    target: ControlTarget,
    mut frames: manifold_ipc::CriticalConsumer,
    notifications: manifold_ipc::EventNotifier,
    proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
) -> Result<()> {
    let _worker = thread::Builder::new()
        .name("manifold-frame-updates".to_string())
        .spawn(move || {
            let mut resources = HashSet::new();
            let mut pending_scene: Option<PendingScene> = None;
            loop {
                if notifications.wait().is_err() {
                    break;
                }
                while let Some(record) = frames.try_recv() {
                    match record.opcode {
                        manifold_ipc::FRAME_SCENE_BEGIN => {
                            pending_scene = Some(PendingScene {
                                sequence: record.sequence,
                                expected_windows: usize::try_from(record.arguments[0])
                                    .unwrap_or(usize::MAX),
                                width: u32::try_from(record.arguments[1]).unwrap_or(u32::MAX),
                                height: u32::try_from(record.arguments[2]).unwrap_or(u32::MAX),
                                windows: Vec::new(),
                            });
                        }
                        manifold_ipc::FRAME_WINDOW_DMABUF => {
                            let Some(scene) = pending_scene.as_mut() else {
                                continue;
                            };
                            if scene.sequence != record.sequence {
                                let _send_result =
                                    proxy.send_event(ClientEvent::TransportError(format!(
                                        "window record sequence {} does not match scene {}",
                                        record.sequence, scene.sequence
                                    )));
                                return;
                            }
                            let resource_id = record.arguments[1];
                            let resource = if resources.contains(&resource_id) {
                                None
                            } else {
                                match dmabuf_resource(&target, resource_id) {
                                    Ok(resource) => {
                                        resources.insert(resource_id);
                                        Some(resource)
                                    }
                                    Err(error) => {
                                        let _send_result =
                                            proxy.send_event(ClientEvent::TransportError(format!(
                                                "fetch DMA-BUF resource {resource_id}: {error:#}"
                                            )));
                                        return;
                                    }
                                }
                            };
                            let (x, y) = unpack_i32_pair(record.arguments[2]);
                            let (width, height) = unpack_i32_pair(record.arguments[3]);
                            scene.windows.push(SceneWindowUpdate {
                                window_id: record.arguments[0],
                                resource_id,
                                rect: [x, y, width, height],
                                active: record.arguments[4] != 0,
                                resource,
                            });
                        }
                        manifold_ipc::FRAME_SCENE_END => {
                            let Some(scene) = pending_scene.take() else {
                                continue;
                            };
                            if scene.sequence != record.sequence
                                || scene.windows.len() != scene.expected_windows
                            {
                                let _send_result =
                                    proxy.send_event(ClientEvent::TransportError(format!(
                                        "incomplete scene {}: expected {} windows, received {}",
                                        scene.sequence,
                                        scene.expected_windows,
                                        scene.windows.len()
                                    )));
                                return;
                            }
                            if proxy
                                .send_event(ClientEvent::SceneFrame {
                                    sequence: scene.sequence,
                                    width: scene.width,
                                    height: scene.height,
                                    windows: scene.windows,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        _ => {}
                    }
                }
            }
        })
        .context("spawn DMA-BUF frame update worker")?;
    Ok(())
}

fn unpack_i32_pair(value: u64) -> (i32, i32) {
    let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or_default();
    let high = u32::try_from(value >> 32).unwrap_or_default();
    (
        i32::from_ne_bytes(low.to_ne_bytes()),
        i32::from_ne_bytes(high.to_ne_bytes()),
    )
}

struct InputLane {
    producer: manifold_ipc::CriticalProducer,
    notifications: manifold_ipc::EventNotifier,
    generation: u64,
    sequence: u64,
}

#[derive(Default)]
struct Application {
    renderer: Option<Renderer>,
    error: Option<anyhow::Error>,
    input_sender: Option<InputLane>,
    fast_path_enabled: bool,
    backend_connected: bool,
    pending_frame: Option<ClientEvent>,
    last_presented_sequence: Option<u64>,
    modifiers: ModifiersState,
    cursor_position: Option<(f64, f64)>,
    pending_plugin_pointer_motion: Option<(f32, f32)>,
    wasm_runtime: Option<PresentationWasmRuntime>,
    plugin_graphics: Option<PluginGraphicsBridge>,
    event_proxy: Option<winit::event_loop::EventLoopProxy<ClientEvent>>,
    next_renderer_generation: u64,
    renderer_recoveries: u8,
}

impl Application {
    fn accept_frame(&mut self, event: ClientEvent) -> Result<()> {
        let (sequence, width, height, windows) = match event {
            ClientEvent::SceneFrame {
                sequence,
                width,
                height,
                windows,
            } => (sequence, width, height, windows),
            ClientEvent::TransportError(message) => return Err(anyhow!(message)),
            ClientEvent::PluginGraphicsChanged { .. } => {
                return Err(anyhow!(
                    "plugin graphics event reached scene transport path"
                ));
            }
            ClientEvent::RendererFailed { .. } => {
                return Err(anyhow!(
                    "renderer failure event reached scene transport path"
                ));
            }
        };
        let Some(renderer) = self.renderer.as_mut() else {
            self.pending_frame = Some(ClientEvent::SceneFrame {
                sequence,
                width,
                height,
                windows,
            });
            return Ok(());
        };
        renderer.select_scene(sequence, width, height, windows)
    }

    fn drain_wasm_events(&mut self, event_loop: &ActiveEventLoop) -> bool {
        if let Some(runtime) = self.wasm_runtime.as_mut()
            && let Err(error) = runtime.drain_events()
        {
            self.error = Some(error);
            event_loop.exit();
            return false;
        }
        true
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: anyhow::Error) {
        self.error = Some(error);
        event_loop.exit();
    }

    fn allocate_renderer_generation(&mut self) -> Result<u64> {
        self.next_renderer_generation = self
            .next_renderer_generation
            .checked_add(1)
            .context("renderer generation limit exceeded")?;
        Ok(self.next_renderer_generation)
    }

    fn recover_renderer(&mut self, failed_generation: u64, message: &str) -> Result<()> {
        let Some(current) = self.renderer.as_ref() else {
            return Ok(());
        };
        if current.generation != failed_generation {
            return Ok(());
        }
        self.renderer_recoveries = self.renderer_recoveries.saturating_add(1);
        if self.renderer_recoveries > 3 {
            return Err(anyhow!(
                "renderer recovery failed repeatedly after: {message}"
            ));
        }
        let old = self
            .renderer
            .take()
            .context("lost renderer during recovery")?;
        let replay = old
            .plugin_pane
            .as_ref()
            .map(|plugin| (plugin.scene.pane, plugin.scene.sequence));
        let window = Arc::clone(&old.window);
        drop(old);
        let generation = self.allocate_renderer_generation()?;
        let proxy = self
            .event_proxy
            .clone()
            .context("renderer recovery event proxy is unavailable")?;
        eprintln!(
            "recovering renderer generation={failed_generation} replacement={generation}: {message}"
        );
        let graphics = self
            .plugin_graphics
            .clone()
            .context("plugin graphics bridge is unavailable")?;
        let mut replacement =
            Renderer::new(window, self.fast_path_enabled, graphics, generation, proxy)?;
        if let Some((pane, sequence)) = replay {
            replacement.refresh_plugin_graphics(pane, sequence)?;
        }
        replacement.window.request_redraw();
        self.renderer = Some(replacement);
        Ok(())
    }
}

impl ApplicationHandler<ClientEvent> for Application {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }

        let title = if self.backend_connected {
            "Manifold"
        } else {
            "Manifold - No backend"
        };
        let generation = match self.allocate_renderer_generation() {
            Ok(generation) => generation,
            Err(error) => {
                self.fail(event_loop, error);
                return;
            }
        };
        let Some(proxy) = self.event_proxy.clone() else {
            self.fail(event_loop, anyhow!("renderer event proxy is unavailable"));
            return;
        };
        let result = event_loop
            .create_window(Window::default_attributes().with_title(title))
            .context("create presentation window")
            .and_then(|window| {
                let graphics = self
                    .plugin_graphics
                    .clone()
                    .context("plugin graphics bridge is unavailable")?;
                Renderer::new(
                    Arc::new(window),
                    self.fast_path_enabled,
                    graphics,
                    generation,
                    proxy,
                )
            });

        match result {
            Ok(renderer) => {
                renderer.window.request_redraw();
                self.renderer = Some(renderer);
                if let (Some(sender), Some(renderer)) =
                    (self.input_sender.as_mut(), self.renderer.as_ref())
                {
                    send_presentation_configuration(sender, renderer);
                }
                if let Some(frame) = self.pending_frame.take()
                    && let Err(error) = self.accept_frame(frame)
                {
                    self.error = Some(error);
                    event_loop.exit();
                }
                if let Some(renderer) = self.renderer.as_ref() {
                    let (width, height, scale) = renderer.logical_extent();
                    if let Err(error) =
                        send_presentation_events(self.wasm_runtime.as_ref(), |pane| {
                            PresentationEvent::PaneCreated {
                                pane,
                                width,
                                height,
                                scale,
                            }
                        })
                    {
                        self.fail(event_loop, error);
                        return;
                    }
                    if let Err(error) =
                        send_presentation_events(self.wasm_runtime.as_ref(), |pane| {
                            PresentationEvent::PaneFocused {
                                pane,
                                focused: true,
                            }
                        })
                    {
                        self.fail(event_loop, error);
                    }
                }
            }
            Err(error) => {
                self.error = Some(error);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: ClientEvent) {
        if !self.drain_wasm_events(event_loop) {
            return;
        }
        match event {
            ClientEvent::PluginGraphicsChanged { pane, sequence } => {
                if let Some(renderer) = self.renderer.as_mut()
                    && let Err(error) = renderer.refresh_plugin_graphics(pane, sequence)
                {
                    self.fail(event_loop, error);
                }
            }
            ClientEvent::RendererFailed {
                generation,
                message,
            } => {
                if let Err(error) = self.recover_renderer(generation, &message) {
                    self.fail(event_loop, error);
                }
            }
            event => {
                if let Err(error) = self.accept_frame(event) {
                    self.fail(event_loop, error);
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if !self.drain_wasm_events(event_loop) {
            return;
        }
        // A full actor lane keeps the latest position for the next Winit turn.
        // Motion is state, so intermediate samples are disposable.
        let _motion_result = flush_presentation_pointer_motion(
            self.wasm_runtime.as_ref(),
            &mut self.pending_plugin_pointer_motion,
        );
        if self
            .wasm_runtime
            .as_ref()
            .is_some_and(|runtime| !runtime.is_empty())
        {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(50),
            ));
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        if renderer.window.id() != window_id {
            return;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Err(error) = renderer.resize(size.width, size.height) {
                    self.fail(event_loop, error);
                    return;
                }
                let (width, height, scale) = renderer.logical_extent();
                if let Err(error) = send_presentation_events(self.wasm_runtime.as_ref(), |pane| {
                    PresentationEvent::PaneResized {
                        pane,
                        width,
                        height,
                        scale,
                    }
                }) {
                    self.fail(event_loop, error);
                    return;
                }
                if let Some(sender) = self.input_sender.as_mut() {
                    send_presentation_configuration(sender, renderer);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = Some((position.x, position.y));
                let scale = renderer.window.scale_factor() as f32;
                let x = position.x as f32 / scale;
                let y = position.y as f32 / scale;
                self.pending_plugin_pointer_motion = Some((x, y));
                if let (Some(sender), Some((content_width, content_height))) =
                    (self.input_sender.as_mut(), renderer.content_extent())
                {
                    let window_size = renderer.window.inner_size();
                    let x =
                        position.x * f64::from(content_width) / f64::from(window_size.width.max(1));
                    let y = position.y * f64::from(content_height)
                        / f64::from(window_size.height.max(1));
                    let (x, y) = renderer
                        .backend_position_for_client_point(x, y)
                        .unwrap_or((x, y));
                    send_input(
                        sender,
                        manifold_ipc::INPUT_POINTER_MOTION,
                        [x.to_bits(), y.to_bits(), 0, 0, 0],
                    );
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let (x, y) = self.cursor_position.unwrap_or_default();
                let scale = renderer.window.scale_factor() as f32;
                let x = x as f32 / scale;
                let y = y as f32 / scale;
                let button_code = linux_button_code(button).unwrap_or_default();
                let pressed = state == ElementState::Pressed;
                let modifiers = modifier_bits(self.modifiers);
                if let Err(error) = send_presentation_events(self.wasm_runtime.as_ref(), |pane| {
                    PresentationEvent::PointerButton {
                        pane,
                        x,
                        y,
                        button: button_code,
                        pressed,
                        modifiers,
                    }
                }) {
                    eprintln!("dropped plugin pointer button: {error:#}");
                }
                if state == ElementState::Pressed
                    && button == MouseButton::Left
                    && let Some((x, y)) = self.cursor_position
                {
                    let window_size = renderer.window.inner_size();
                    let scene_x = x * f64::from(renderer.content_extent().map_or(1, |size| size.0))
                        / f64::from(window_size.width.max(1));
                    let scene_y = y * f64::from(renderer.content_extent().map_or(1, |size| size.1))
                        / f64::from(window_size.height.max(1));
                    if let Some(window_id) = renderer.focus_window_at(scene_x, scene_y)
                        && let Some(sender) = self.input_sender.as_mut()
                    {
                        send_window_focus(sender, window_id);
                    }
                }
                if let (Some(sender), Some(code)) =
                    (self.input_sender.as_mut(), linux_button_code(button))
                {
                    send_input(
                        sender,
                        manifold_ipc::INPUT_POINTER_BUTTON,
                        [
                            u64::from(code),
                            u64::from(state == ElementState::Pressed),
                            0,
                            0,
                            0,
                        ],
                    );
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (horizontal, vertical) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (f64::from(x) * 15.0, f64::from(y) * 15.0),
                    MouseScrollDelta::PixelDelta(position) => (position.x, position.y),
                };
                let (x, y) = self.cursor_position.unwrap_or_default();
                let scale = renderer.window.scale_factor() as f32;
                let logical_horizontal = horizontal as f32 / scale;
                let logical_vertical = vertical as f32 / scale;
                let x = x as f32 / scale;
                let y = y as f32 / scale;
                let modifiers = modifier_bits(self.modifiers);
                if let Err(error) = send_presentation_events(self.wasm_runtime.as_ref(), |pane| {
                    PresentationEvent::PointerScroll {
                        pane,
                        x,
                        y,
                        horizontal: logical_horizontal,
                        vertical: logical_vertical,
                        modifiers,
                    }
                }) {
                    eprintln!("dropped plugin pointer scroll: {error:#}");
                }
                if let Some(sender) = self.input_sender.as_mut() {
                    send_input(
                        sender,
                        manifold_ipc::INPUT_POINTER_AXIS,
                        [horizontal.to_bits(), vertical.to_bits(), 0, 0, 0],
                    );
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::Focused(focused) => {
                if let Err(error) = send_presentation_events(self.wasm_runtime.as_ref(), |pane| {
                    PresentationEvent::PaneFocused { pane, focused }
                }) {
                    eprintln!("dropped plugin focus event: {error:#}");
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } if !is_synthetic => {
                let modifiers = modifier_bits(self.modifiers);
                if let Some(physical_key) = event.physical_key.to_scancode()
                    && let Err(error) =
                        send_presentation_events(self.wasm_runtime.as_ref(), |pane| {
                            PresentationEvent::Key {
                                pane,
                                physical_key,
                                pressed: event.state == ElementState::Pressed,
                                repeat: event.repeat,
                                modifiers,
                            }
                        })
                {
                    eprintln!("dropped plugin key event: {error:#}");
                }
                if event.state == ElementState::Pressed
                    && let Some(text) = event.text.as_ref()
                    && !text.is_empty()
                {
                    let text = text.to_string();
                    if let Err(error) =
                        send_presentation_events(self.wasm_runtime.as_ref(), |pane| {
                            PresentationEvent::Text {
                                pane,
                                text: text.clone(),
                            }
                        })
                    {
                        eprintln!("dropped plugin text event: {error:#}");
                    }
                }
                if event.repeat {
                    return;
                }
                // TEMPORARY TESTING HACK: Ctrl-J/Ctrl-K locally cycle the client-owned
                // focused pane until presentation shortcut policy is hosted in the client.
                let cycle_offset = if self.modifiers.control_key() {
                    match event.physical_key {
                        PhysicalKey::Code(KeyCode::KeyJ) => Some(1),
                        PhysicalKey::Code(KeyCode::KeyK) => Some(-1),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(offset) = cycle_offset {
                    if event.state == ElementState::Pressed
                        && let Some(window_id) = renderer.cycle_focus(offset)
                        && let Some(sender) = self.input_sender.as_mut()
                    {
                        send_window_focus(sender, window_id);
                    }
                    return;
                }
                if let (Some(sender), Some(scancode)) =
                    (self.input_sender.as_mut(), event.physical_key.to_scancode())
                {
                    send_input(
                        sender,
                        manifold_ipc::INPUT_KEYBOARD_KEY,
                        [
                            u64::from(scancode + 8),
                            u64::from(event.state == ElementState::Pressed),
                            0,
                            0,
                            0,
                        ],
                    );
                }
            }
            WindowEvent::RedrawRequested => match renderer.render() {
                RenderOutcome::Presented {
                    sequence,
                    reconfigure,
                } => {
                    self.renderer_recoveries = 0;
                    if reconfigure {
                        renderer.reconfigure();
                    }
                    if let Some(sequence) = sequence
                        && self.last_presented_sequence != Some(sequence)
                        && let Some(sender) = self.input_sender.as_mut()
                    {
                        if send_input(
                            sender,
                            manifold_ipc::INPUT_FRAME_PRESENTED,
                            [sequence, 0, 0, 0, 0],
                        ) {
                            self.last_presented_sequence = Some(sequence);
                        } else {
                            renderer.window.request_redraw();
                        }
                    }
                }
                RenderOutcome::Skipped => {}
                RenderOutcome::Reconfigure => renderer.reconfigure(),
                RenderOutcome::Failed(message) => {
                    let generation = renderer.generation;
                    if let Err(error) = self.recover_renderer(generation, message) {
                        self.fail(event_loop, error);
                    }
                }
            },
            _ => {}
        }
    }
}

struct Renderer {
    generation: u64,
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    frame_bind_group_layout: wgpu::BindGroupLayout,
    frame_sampler: wgpu::Sampler,
    frame_pipeline: wgpu::RenderPipeline,
    plugin_canvas: GpuCanvasRenderer,
    imported_frames: HashMap<u64, ImportedFrame>,
    current_windows: Vec<SceneWindow>,
    scene_extent: Option<(u32, u32)>,
    current_sequence: Option<u64>,
    plugin_graphics: PluginGraphicsBridge,
    plugin_pane: Option<PluginPaneTexture>,
}

struct SceneWindow {
    window_id: u64,
    resource_id: u64,
    source_rect: [i32; 4],
    rect: [i32; 4],
    active: bool,
}

struct ImportedFrame {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

struct PluginPaneTexture {
    scene: GpuCanvasScene,
}

fn send_presentation_configuration(lane: &mut InputLane, renderer: &Renderer) {
    let size = renderer.window.inner_size();
    let refresh_millihz = renderer
        .window
        .current_monitor()
        .and_then(|monitor| monitor.refresh_rate_millihertz())
        .unwrap_or(60_000);
    send_input(
        lane,
        manifold_ipc::INPUT_PRESENTATION_SIZE,
        [
            u64::from(size.width.max(1)),
            u64::from(size.height.max(1)),
            u64::from(refresh_millihz),
            0,
            0,
        ],
    );
}

fn send_input(lane: &mut InputLane, opcode: u32, arguments: [u64; 5]) -> bool {
    lane.sequence = lane.sequence.wrapping_add(1);
    if lane
        .producer
        .try_send(manifold_ipc::CriticalRecord {
            opcode,
            connection_generation: lane.generation,
            sequence: lane.sequence,
            arguments,
            ..manifold_ipc::CriticalRecord::default()
        })
        .is_err()
    {
        // Input remains non-blocking if the bounded consumer falls behind.
        return false;
    }
    if lane.notifications.notify().is_err() {
        return false;
    }
    true
}

fn send_window_focus(lane: &mut InputLane, window_id: u64) {
    let _sent = send_input(
        lane,
        manifold_ipc::INPUT_WINDOW_FOCUS,
        [window_id, 0, 0, 0, 0],
    );
}

fn send_presentation_events(
    runtime: Option<&PresentationWasmRuntime>,
    mut create: impl FnMut(u64) -> PresentationEvent,
) -> Result<()> {
    let Some(runtime) = runtime else {
        return Ok(());
    };
    let panes = runtime.pane_ids().collect::<Vec<_>>();
    for pane in panes {
        runtime.send_presentation_event(create(pane))?;
    }
    Ok(())
}

fn flush_presentation_pointer_motion(
    runtime: Option<&PresentationWasmRuntime>,
    pending: &mut Option<(f32, f32)>,
) -> Result<()> {
    let Some((x, y)) = *pending else {
        return Ok(());
    };
    send_presentation_events(runtime, |pane| PresentationEvent::PointerMoved {
        pane,
        x,
        y,
    })?;
    pending.take();
    Ok(())
}

const fn linux_button_code(button: MouseButton) -> Option<u32> {
    match button {
        MouseButton::Left => Some(0x110),
        MouseButton::Right => Some(0x111),
        MouseButton::Middle => Some(0x112),
        MouseButton::Back => Some(0x116),
        MouseButton::Forward => Some(0x115),
        MouseButton::Other(_) => None,
    }
}

fn modifier_bits(modifiers: ModifiersState) -> u32 {
    u32::from(modifiers.shift_key())
        | (u32::from(modifiers.control_key()) << 1)
        | (u32::from(modifiers.alt_key()) << 2)
        | (u32::from(modifiers.super_key()) << 3)
}

impl ImportedFrame {
    fn new(
        device: &wgpu::Device,
        bind_group_layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        window_id: u64,
        resource_id: u64,
        frame: DmabufFrame,
    ) -> Result<Self> {
        if frame.color_space != manifold_ipc::FrameColorSpace::Srgb {
            return Err(anyhow!(
                "unsupported happy-path DMA-BUF color space {:?}",
                frame.color_space
            ));
        }
        let (format, view_format) = dmabuf_texture_formats(frame.fourcc)?;
        let texture = import_single_plane(
            device,
            frame.fd,
            SinglePlaneDmabuf {
                width: frame.width,
                height: frame.height,
                format,
                view_format: Some(view_format),
                modifier: frame.modifier,
                stride: u64::from(frame.stride),
                offset: u64::from(frame.offset),
            },
        )?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("hosted DMA-BUF sRGB view"),
            format: Some(view_format),
            ..Default::default()
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("hosted DMA-BUF bind group"),
            layout: bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        eprintln!(
            "imported window DMA-BUF window_id={} resource_id={} size={}x{} format={:?} color_space={:?} stride={} modifier={} y_inverted={}",
            window_id,
            resource_id,
            frame.width,
            frame.height,
            format,
            frame.color_space,
            frame.stride,
            frame.modifier,
            frame.y_inverted
        );
        Ok(Self {
            _texture: texture,
            bind_group,
        })
    }
}

fn create_frame_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
) -> (wgpu::BindGroupLayout, wgpu::Sampler, wgpu::RenderPipeline) {
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("hosted DMA-BUF sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("hosted DMA-BUF layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("hosted DMA-BUF shader"),
        source: wgpu::ShaderSource::Wgsl(
            "struct VertexOutput {
                    @builtin(position) position: vec4<f32>,
                    @location(0) uv: vec2<f32>,
                };
                @vertex fn vertex(@builtin(vertex_index) index: u32) -> VertexOutput {
                    var positions = array<vec2<f32>, 3>(
                        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
                    var uvs = array<vec2<f32>, 3>(
                        vec2<f32>(0.0, 0.0), vec2<f32>(2.0, 0.0), vec2<f32>(0.0, 2.0));
                    var output: VertexOutput;
                    output.position = vec4<f32>(positions[index], 0.0, 1.0);
                    output.uv = uvs[index];
                    return output;
                }
                @group(0) @binding(0) var image: texture_2d<f32>;
                @group(0) @binding(1) var image_sampler: sampler;

                fn linear_to_srgb_channel(linear: f32) -> f32 {
                    return select(
                        12.92 * linear,
                        1.055 * pow(max(linear, 0.0), 1.0 / 2.4) - 0.055,
                        linear > 0.0031308);
                }
                fn linear_to_srgb(linear: vec3<f32>) -> vec3<f32> {
                    return vec3<f32>(
                        linear_to_srgb_channel(linear.r),
                        linear_to_srgb_channel(linear.g),
                        linear_to_srgb_channel(linear.b));
                }
                @fragment fn fragment_linear_target(
                    input: VertexOutput
                ) -> @location(0) vec4<f32> {
                    return textureSample(image, image_sampler, input.uv);
                }
                @fragment fn fragment_encoded_target(
                    input: VertexOutput
                ) -> @location(0) vec4<f32> {
                    let linear = textureSample(image, image_sampler, input.uv);
                    return vec4<f32>(linear_to_srgb(linear.rgb), linear.a);
                }"
            .into(),
        ),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("hosted DMA-BUF pipeline layout"),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("hosted DMA-BUF pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vertex"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some(if target_format.is_srgb() {
                "fragment_linear_target"
            } else {
                "fragment_encoded_target"
            }),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    (bind_group_layout, sampler, pipeline)
}

fn dmabuf_texture_formats(fourcc: u32) -> Result<(wgpu::TextureFormat, wgpu::TextureFormat)> {
    const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
    const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
    const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;
    const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258;
    match fourcc {
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => Ok((
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
        )),
        DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888 => Ok((
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        )),
        _ => Err(anyhow!("unsupported happy-path DMA-BUF fourcc {fourcc:#x}")),
    }
}

fn srgb_surface_format(
    supported: &[wgpu::SurfaceFormatCapabilities],
) -> Option<wgpu::TextureFormat> {
    let mut encoded_fallback = None;
    for capability in supported {
        if !capability
            .color_spaces
            .contains(wgpu::SurfaceColorSpaces::SRGB)
        {
            continue;
        }
        if capability.format.is_srgb() {
            return Some(capability.format);
        }
        if encoded_fallback.is_none() {
            encoded_fallback = Some(capability.format);
        }
    }
    encoded_fallback
}

impl Renderer {
    fn new(
        window: Arc<Window>,
        fast_path_enabled: bool,
        plugin_graphics: PluginGraphicsBridge,
        generation: u64,
        proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
    ) -> Result<Self> {
        pollster::block_on(Self::new_async(
            window,
            fast_path_enabled,
            plugin_graphics,
            generation,
            proxy,
        ))
    }

    async fn new_async(
        window: Arc<Window>,
        fast_path_enabled: bool,
        plugin_graphics: PluginGraphicsBridge,
        generation: u64,
        proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
    ) -> Result<Self> {
        let instance = if fast_path_enabled {
            let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
            descriptor.backends = wgpu::Backends::VULKAN;
            wgpu::Instance::new(descriptor)
        } else {
            wgpu::Instance::default()
        };
        let surface = instance
            .create_surface(Arc::clone(&window))
            .context("create wgpu surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .context("find a wgpu adapter")?;
        let required_features = if fast_path_enabled {
            wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF
        } else {
            wgpu::Features::empty()
        };
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("manifold-client device"),
                required_features,
                ..Default::default()
            })
            .await
            .context("create wgpu device")?;
        let lost_proxy = proxy.clone();
        device.set_device_lost_callback(move |reason, message| {
            let _send_result = lost_proxy.send_event(ClientEvent::RendererFailed {
                generation,
                message: format!("wgpu device lost ({reason:?}): {message}"),
            });
        });
        device.on_uncaptured_error(Arc::new(move |error| {
            let _send_result = proxy.send_event(ClientEvent::RendererFailed {
                generation,
                message: format!("uncaptured wgpu error: {error}"),
            });
        }));

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| anyhow!("wgpu surface has no supported configuration"))?;
        let capabilities = surface.get_capabilities(&adapter);
        config.format = srgb_surface_format(&capabilities.format_capabilities)
            .ok_or_else(|| anyhow!("wgpu surface cannot present standard sRGB"))?;
        config.color_space = wgpu::SurfaceColorSpace::Srgb;
        config.present_mode = low_latency_present_mode(&capabilities.present_modes);
        config.desired_maximum_frame_latency = 1;
        let (frame_bind_group_layout, frame_sampler, frame_pipeline) =
            create_frame_pipeline(&device, config.format);
        let plugin_canvas =
            GpuCanvasRenderer::new(&device, &queue, &frame_bind_group_layout, &frame_sampler);
        surface.configure(&device, &config);
        eprintln!(
            "configured presentation surface format={:?} color_space={:?}",
            config.format, config.color_space
        );
        Ok(Self {
            generation,
            window,
            surface,
            device,
            queue,
            config,
            frame_bind_group_layout,
            frame_sampler,
            frame_pipeline,
            plugin_canvas,
            imported_frames: HashMap::new(),
            current_windows: Vec::new(),
            scene_extent: None,
            current_sequence: None,
            plugin_graphics,
            plugin_pane: None,
        })
    }

    fn logical_extent(&self) -> (f32, f32, f32) {
        let scale = self.window.scale_factor() as f32;
        (
            self.config.width as f32 / scale,
            self.config.height as f32 / scale,
            scale,
        )
    }

    const fn content_extent(&self) -> Option<(u32, u32)> {
        self.scene_extent
    }

    fn select_scene(
        &mut self,
        sequence: u64,
        width: u32,
        height: u32,
        windows: Vec<SceneWindowUpdate>,
    ) -> Result<()> {
        let previous_focus = self
            .current_windows
            .iter()
            .find(|window| window.active)
            .map(|window| window.window_id);
        let mut scene_windows = Vec::with_capacity(windows.len());
        for window in windows {
            if let Some(frame) = window.resource {
                let imported = ImportedFrame::new(
                    &self.device,
                    &self.frame_bind_group_layout,
                    &self.frame_sampler,
                    window.window_id,
                    window.resource_id,
                    frame,
                )?;
                self.imported_frames.insert(window.resource_id, imported);
            }
            scene_windows.push(SceneWindow {
                window_id: window.window_id,
                resource_id: window.resource_id,
                source_rect: window.rect,
                rect: window.rect,
                active: window.active,
            });
        }
        scene_windows.sort_by_key(|window| window.window_id);
        let focused_window = previous_focus
            .filter(|focused| {
                scene_windows
                    .iter()
                    .any(|window| window.window_id == *focused)
            })
            .or_else(|| {
                scene_windows
                    .iter()
                    .find(|window| window.active)
                    .map(|window| window.window_id)
            })
            .or_else(|| scene_windows.first().map(|window| window.window_id));
        map_windows(
            &mut scene_windows,
            LayoutSize {
                w: i32::try_from(width).unwrap_or(i32::MAX).max(1),
                h: i32::try_from(height).unwrap_or(i32::MAX).max(1),
            },
            focused_window,
        );
        if scene_windows
            .iter()
            .all(|window| self.imported_frames.contains_key(&window.resource_id))
        {
            self.current_windows = scene_windows;
            self.scene_extent = Some((width.max(1), height.max(1)));
            self.current_sequence = Some(sequence);
            self.window.request_redraw();
        }
        Ok(())
    }

    fn cycle_focus(&mut self, offset: isize) -> Option<u64> {
        let window_count = self.current_windows.len();
        if window_count < 2 {
            return None;
        }
        let active_index = self
            .current_windows
            .iter()
            .position(|window| window.active)
            .unwrap_or_default();
        let next_index = cycled_index(active_index, window_count, offset)?;
        self.focus_window_index(next_index)
    }

    fn focus_window_at(&mut self, x: f64, y: f64) -> Option<u64> {
        let index = self
            .current_windows
            .iter()
            .rposition(|window| rect_contains(window.rect, x, y))?;
        self.focus_window_index(index)
    }

    fn focus_window_index(&mut self, index: usize) -> Option<u64> {
        let window_id = self.current_windows.get(index)?.window_id;
        for window in &mut self.current_windows {
            window.active = window.window_id == window_id;
        }
        self.window.request_redraw();
        Some(window_id)
    }

    fn backend_position_for_client_point(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let window = self
            .current_windows
            .iter()
            .rev()
            .find(|window| rect_contains(window.rect, x, y))?;
        Some(project_point_between_rects(
            x,
            y,
            window.rect,
            window.source_rect,
        ))
    }

    fn refresh_plugin_graphics(&mut self, pane: u64, expected_sequence: u64) -> Result<()> {
        let Some(snapshot) = self.plugin_graphics.snapshot(pane)? else {
            return Ok(());
        };
        if snapshot.sequence < expected_sequence {
            return Ok(());
        }
        if self.plugin_pane.as_ref().is_some_and(|current| {
            current.scene.pane == snapshot.pane
                && current.scene.sequence >= snapshot.sequence
                && current.scene.width == self.config.width
                && current.scene.height == self.config.height
        }) {
            return Ok(());
        }
        let scale = self.window.scale_factor() as f32;
        let scene = self.plugin_canvas.compile(
            &self.device,
            &self.queue,
            &snapshot,
            (self.config.width.max(1), self.config.height.max(1), scale),
            self.plugin_pane.as_ref().map(|current| &current.scene),
        )?;
        if snapshot.sequence <= 3 || snapshot.sequence.is_multiple_of(120) {
            eprintln!(
                "accepted plugin draw packet pane={} sequence={} commands={} gpu_target={}x{} scale={:.2}",
                snapshot.pane,
                snapshot.sequence,
                snapshot.packet.commands.len(),
                self.config.width,
                self.config.height,
                scale,
            );
        }
        self.plugin_pane = Some(PluginPaneTexture { scene });
        self.window.request_redraw();
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }

        self.config.width = width;
        self.config.height = height;
        self.reconfigure();
        if let Some((pane, sequence)) = self
            .plugin_pane
            .as_ref()
            .map(|plugin| (plugin.scene.pane, plugin.scene.sequence))
        {
            self.refresh_plugin_graphics(pane, sequence)?;
        }
        self.window.request_redraw();
        Ok(())
    }

    fn reconfigure(&self) {
        self.surface.configure(&self.device, &self.config);
        self.window.request_redraw();
    }

    fn render(&mut self) -> RenderOutcome {
        let (frame, reconfigure) = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(frame) => (frame, false),
            CurrentSurfaceTexture::Suboptimal(frame) => (frame, true),
            CurrentSurfaceTexture::Timeout | CurrentSurfaceTexture::Occluded => {
                return RenderOutcome::Skipped;
            }
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                return RenderOutcome::Reconfigure;
            }
            CurrentSurfaceTexture::Validation => {
                return RenderOutcome::Failed("wgpu surface validation failed");
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("manifold-client frame encoder"),
            });

        if let Some(plugin) = self.plugin_pane.as_mut()
            && let Err(error) = self
                .plugin_canvas
                .encode_if_dirty(&mut encoder, &mut plugin.scene)
        {
            eprintln!("failed to encode plugin GPU scene: {error:#}");
            return RenderOutcome::Failed("failed to encode plugin GPU scene");
        }

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("manifold-client clear pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(Color {
                            r: 0.04,
                            g: 0.05,
                            b: 0.07,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            render_pass.set_pipeline(&self.frame_pipeline);
            if let Some(plugin) = self.plugin_pane.as_ref() {
                render_pass.set_viewport(
                    0.0,
                    0.0,
                    self.config.width as f32,
                    self.config.height as f32,
                    0.0,
                    1.0,
                );
                render_pass.set_bind_group(0, plugin.scene.output_bind_group(), &[]);
                render_pass.draw(0..3, 0..1);
            }
            if let Some((scene_width, scene_height)) = self.scene_extent {
                let scene_width_i32 = i32::try_from(scene_width).unwrap_or(i32::MAX);
                let scene_height_i32 = i32::try_from(scene_height).unwrap_or(i32::MAX);
                for window in &self.current_windows {
                    let Some(imported) = self.imported_frames.get(&window.resource_id) else {
                        continue;
                    };
                    let [x, y, width, height] = window.rect;
                    let x = x.clamp(0, scene_width_i32);
                    let y = y.clamp(0, scene_height_i32);
                    let width = width.max(0).min(scene_width_i32 - x);
                    let height = height.max(0).min(scene_height_i32 - y);
                    if width == 0 || height == 0 {
                        continue;
                    }
                    let viewport = scaled_viewport(
                        [x, y, width, height],
                        (scene_width, scene_height),
                        (self.config.width, self.config.height),
                    );
                    render_pass.set_viewport(
                        viewport[0],
                        viewport[1],
                        viewport[2],
                        viewport[3],
                        0.0,
                        1.0,
                    );
                    render_pass.set_bind_group(0, &imported.bind_group, &[]);
                    render_pass.draw(0..3, 0..1);
                }
            }
            drop(render_pass);
        }

        let submission = self.queue.submit([encoder.finish()]);
        if self.current_sequence.is_some()
            && self
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission),
                    timeout: Some(std::time::Duration::from_secs(1)),
                })
                .is_err()
        {
            return RenderOutcome::Failed("timed out waiting for composed frame consumption");
        }
        self.queue.present(frame);
        RenderOutcome::Presented {
            sequence: self.current_sequence,
            reconfigure,
        }
    }
}

fn map_windows(windows: &mut [SceneWindow], extent: LayoutSize, focused_window: Option<u64>) {
    let mut layout = LayoutState::default();
    for _ in 1..windows.len() {
        let active_rect = layout.active_rect(extent).unwrap_or(LayoutRect {
            x: 0,
            y: 0,
            w: extent.w,
            h: extent.h,
        });
        let axis = if active_rect.w >= active_rect.h {
            LayoutAxis::Horizontal
        } else {
            LayoutAxis::Vertical
        };
        let Some(next_leaf) = layout.split_active(axis) else {
            break;
        };
        layout.set_active_leaf(next_leaf);
    }
    for (window, region) in windows.iter_mut().zip(layout.resolve(extent)) {
        window.rect = [region.rect.x, region.rect.y, region.rect.w, region.rect.h];
        window.active = focused_window == Some(window.window_id);
    }
}

fn cycled_index(current: usize, count: usize, offset: isize) -> Option<usize> {
    if count == 0 || current >= count {
        return None;
    }
    let step = offset.unsigned_abs() % count;
    Some(if offset.is_negative() {
        current
            .checked_sub(step)
            .unwrap_or_else(|| count - (step - current))
    } else {
        (current + step) % count
    })
}

fn rect_contains(rect: [i32; 4], x: f64, y: f64) -> bool {
    let [left, top, width, height] = rect;
    x >= f64::from(left)
        && y >= f64::from(top)
        && x < f64::from(left.saturating_add(width.max(0)))
        && y < f64::from(top.saturating_add(height.max(0)))
}

fn project_point_between_rects(x: f64, y: f64, from: [i32; 4], to: [i32; 4]) -> (f64, f64) {
    let [from_x, from_y, from_width, from_height] = from;
    let [to_x, to_y, to_width, to_height] = to;
    let local_x = (x - f64::from(from_x)) / f64::from(from_width.max(1));
    let local_y = (y - f64::from(from_y)) / f64::from(from_height.max(1));
    (
        local_x.mul_add(f64::from(to_width.max(1)), f64::from(to_x)),
        local_y.mul_add(f64::from(to_height.max(1)), f64::from(to_y)),
    )
}

#[allow(
    clippy::cast_precision_loss,
    reason = "WGPU viewports require f32 while protocol and surface extents are integer pixels"
)]
fn scaled_viewport(
    rect: [i32; 4],
    scene_extent: (u32, u32),
    target_extent: (u32, u32),
) -> [f32; 4] {
    let [x, y, width, height] = rect;
    let scale_x = target_extent.0 as f32 / scene_extent.0 as f32;
    let scale_y = target_extent.1 as f32 / scene_extent.1 as f32;
    [
        x as f32 * scale_x,
        y as f32 * scale_y,
        width as f32 * scale_x,
        height as f32 * scale_y,
    ]
}

#[derive(Clone, Copy)]
enum RenderOutcome {
    Presented {
        sequence: Option<u64>,
        reconfigure: bool,
    },
    Reconfigure,
    Skipped,
    Failed(&'static str),
}

fn low_latency_present_mode(supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    [
        wgpu::PresentMode::Mailbox,
        wgpu::PresentMode::Immediate,
        wgpu::PresentMode::FifoRelaxed,
        wgpu::PresentMode::Fifo,
    ]
    .into_iter()
    .find(|candidate| supported.contains(candidate))
    .unwrap_or(wgpu::PresentMode::AutoNoVsync)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_is_preferred_for_low_latency_without_tearing() {
        assert_eq!(
            low_latency_present_mode(&[
                wgpu::PresentMode::Fifo,
                wgpu::PresentMode::Immediate,
                wgpu::PresentMode::Mailbox,
            ]),
            wgpu::PresentMode::Mailbox
        );
    }

    #[test]
    fn immediate_is_used_when_mailbox_is_unavailable() {
        assert_eq!(
            low_latency_present_mode(&[wgpu::PresentMode::Fifo, wgpu::PresentMode::Immediate,]),
            wgpu::PresentMode::Immediate
        );
    }

    #[test]
    fn srgb_surface_prefers_a_hardware_encoding_format() {
        let supported = [
            wgpu::SurfaceFormatCapabilities {
                format: wgpu::TextureFormat::Bgra8Unorm,
                color_spaces: wgpu::SurfaceColorSpaces::SRGB,
            },
            wgpu::SurfaceFormatCapabilities {
                format: wgpu::TextureFormat::Bgra8UnormSrgb,
                color_spaces: wgpu::SurfaceColorSpaces::SRGB,
            },
        ];
        assert_eq!(
            srgb_surface_format(&supported),
            Some(wgpu::TextureFormat::Bgra8UnormSrgb)
        );
    }

    #[test]
    fn encoded_srgb_surface_is_a_valid_fallback() {
        let supported = [wgpu::SurfaceFormatCapabilities {
            format: wgpu::TextureFormat::Rgba8Unorm,
            color_spaces: wgpu::SurfaceColorSpaces::SRGB,
        }];
        assert_eq!(
            srgb_surface_format(&supported),
            Some(wgpu::TextureFormat::Rgba8Unorm)
        );
    }

    #[test]
    fn dma_fourcc_preserves_channel_order_while_adding_an_srgb_view() {
        assert_eq!(
            dmabuf_texture_formats(0x3432_5241).ok(),
            Some((
                wgpu::TextureFormat::Bgra8Unorm,
                wgpu::TextureFormat::Bgra8UnormSrgb
            ))
        );
        assert_eq!(
            dmabuf_texture_formats(0x3432_4241).ok(),
            Some((
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::TextureFormat::Rgba8UnormSrgb
            ))
        );
    }

    #[test]
    fn packed_signed_window_coordinates_round_trip() {
        let packed = u64::from(u32::from_ne_bytes((-17_i32).to_ne_bytes()))
            | (u64::from(u32::from_ne_bytes(42_i32.to_ne_bytes())) << 32);
        assert_eq!(unpack_i32_pair(packed), (-17, 42));
    }

    #[test]
    fn window_viewport_scales_with_the_client_surface() {
        let actual = scaled_viewport([500, 0, 500, 750], (1000, 750), (800, 600));
        let expected = [400.0, 0.0, 400.0, 600.0];
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| (actual - expected).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn client_maps_three_windows_with_the_native_split_fallback() {
        let mut windows = [scene_window(11), scene_window(12), scene_window(13)];
        map_windows(&mut windows, LayoutSize { w: 1200, h: 800 }, Some(12));

        assert_eq!(windows[0].rect, [0, 0, 600, 800]);
        assert_eq!(windows[1].rect, [600, 0, 600, 400]);
        assert_eq!(windows[2].rect, [600, 400, 600, 400]);
        assert!(!windows[0].active);
        assert!(windows[1].active);
        assert!(!windows[2].active);
    }

    #[test]
    fn focus_cycle_wraps_in_both_directions() {
        assert_eq!(cycled_index(2, 3, 1), Some(0));
        assert_eq!(cycled_index(0, 3, -1), Some(2));
        assert_eq!(cycled_index(0, 0, 1), None);
    }

    #[test]
    fn client_pointer_position_projects_into_the_matching_backend_window() {
        let actual =
            project_point_between_rects(900.0, 200.0, [600, 0, 600, 400], [0, 400, 600, 400]);
        assert_eq!(actual, (300.0, 600.0));
    }

    fn scene_window(window_id: u64) -> SceneWindow {
        SceneWindow {
            window_id,
            resource_id: window_id,
            source_rect: [0, 0, 1, 1],
            rect: [0, 0, 1, 1],
            active: false,
        }
    }
}
