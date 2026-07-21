#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "bounded window, text, and graphics coordinates intentionally narrow to the f32 and integer formats used by WGPU"
)]

use std::{
    collections::HashMap,
    os::fd::AsFd,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use manifold_cli::{
    ClientArgs,
    control::{
        ControlTarget, DmabufFrame, ServerLaunch, connect_or_spawn, dmabuf_resource,
        open_presentation_fast_path, request_dbus_command, resolve_control_target,
    },
};
use manifold_dmabuf::{SinglePlaneDmabuf, import_single_plane};
use manifold_layout::{
    InputBehavior, LayoutAction, LayoutItemId, LayoutItemKind, LayoutRect, LayoutSize, LocalPoint,
    LocalTransform, ResolvedLayout, ResolvedLayoutItem,
};
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
mod key_policy;
mod layout_policy;
mod plugin_gpu;
mod plugin_graphics;

use actor_runtime::PresentationWasmRuntime;
use key_policy::{ClientKeyPolicy, KeyPolicyAppPane, KeyPolicySnapshot};
use layout_policy::{ClientLayoutPolicy, LayoutSource};
use manifold_wasm::{GlobalKeyDecision, HostMessage, PresentationEvent};
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
    let mut layout_policy = ClientLayoutPolicy::load(args.wasm_layout_policy.as_deref())?;
    let mut key_policy = ClientKeyPolicy::load(args.wasm_key_policy.as_deref())?;
    if args.check_wasm_layout_policy {
        let resolution = layout_policy.resolve(&[1, 2], Some(1), LayoutSize { w: 1200, h: 800 });
        if resolution.source != LayoutSource::WasmPolicy || resolution.layout.pane_count() != 2 {
            return Err(anyhow!(
                "presentation layout policy did not produce the expected two-pane flat layout"
            ));
        }
        eprintln!("presentation WASM layout policy check passed");
        return Ok(());
    }
    if args.check_wasm_key_policy {
        let snapshot = KeyPolicySnapshot {
            focused_pane: Some(1),
            panes: vec![1, 2],
            app_panes: vec![KeyPolicyAppPane {
                pane: 2,
                app_id: "manifold-terminal-1".to_string(),
            }],
        };
        let evaluation = key_policy.evaluate(
            44,
            manifold_wasm::ABI_KEY_PRESSED,
            manifold_wasm::ABI_MOD_CTRL,
            &snapshot,
        )?;
        if evaluation.decision != GlobalKeyDecision::Consume
            || !evaluation.host_messages.iter().any(|message| {
                matches!(
                    message,
                    HostMessage::LayoutAction { action } if action == "focus-next"
                )
            })
        {
            return Err(anyhow!(
                "presentation key policy did not consume Ctrl+J as focus-next"
            ));
        }
        let mode_toggle = key_policy.evaluate(
            56,
            manifold_wasm::ABI_KEY_PRESSED,
            manifold_wasm::ABI_MOD_CTRL,
            &snapshot,
        )?;
        let _mode_toggle_release = key_policy.evaluate(
            56,
            manifold_wasm::ABI_KEY_RELEASED,
            manifold_wasm::ABI_MOD_CTRL,
            &snapshot,
        )?;
        let spawn = key_policy.evaluate(
            32,
            manifold_wasm::ABI_KEY_PRESSED,
            manifold_wasm::ABI_MOD_CTRL,
            &snapshot,
        )?;
        let focus_existing = key_policy.evaluate(
            10,
            manifold_wasm::ABI_KEY_PRESSED,
            manifold_wasm::ABI_MOD_CTRL,
            &snapshot,
        )?;
        if mode_toggle.decision != GlobalKeyDecision::Consume
            || spawn.decision != GlobalKeyDecision::Consume
            || !spawn.host_messages.iter().any(|message| {
                matches!(
                    message,
                    HostMessage::SpawnApp { argv }
                        if argv.first().is_some_and(|program| program == "foot")
                )
            })
            || !focus_existing
                .host_messages
                .iter()
                .any(|message| matches!(message, HostMessage::FocusPane { pane: 2 }))
        {
            return Err(anyhow!(
                "presentation key policy did not preserve mode, spawn, and app-focus behavior"
            ));
        }
        eprintln!("presentation WASM key policy check passed");
        return Ok(());
    }
    let event_loop = EventLoop::<ClientEvent>::with_user_event()
        .build()
        .context("create event loop")?;
    let event_proxy = event_loop.create_proxy();
    let plugin_graphics = PluginGraphicsBridge::new(event_proxy.clone());
    let (input_sender, backend_commands, fast_path_enabled, backend_connected) = if args.no_server {
        eprintln!("starting Manifold presentation without a backend");
        (None, None, false, false)
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
                target.clone(),
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
        let backend_commands = Some(BackendCommandLane::start(
            target,
            event_loop.create_proxy(),
        )?);
        (input_sender, backend_commands, fast_path_enabled, true)
    };
    let mut application = Application {
        wasm_runtime: Some(PresentationWasmRuntime::start(
            &args.wasm_actors,
            Some(&plugin_graphics),
        )?),
        layout_policy: Some(layout_policy),
        key_policy: Some(key_policy),
        plugin_graphics: Some(plugin_graphics),
        input_sender,
        backend_commands,
        fast_path_enabled,
        backend_connected,
        window_focused: true,
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
    let backend_shutdown_result = application
        .backend_commands
        .as_mut()
        .map_or(Ok(()), BackendCommandLane::shutdown)
        .context("stop presentation backend command worker");
    let error = application.error.take();
    drop(application);
    run_result?;
    shutdown_result?;
    backend_shutdown_result?;
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
    BackendCommandCompleted {
        description: String,
        result: Result<String, String>,
    },
}

#[derive(Debug)]
struct SceneWindowUpdate {
    window_id: u64,
    resource_id: u64,
    rect: [i32; 4],
    active: bool,
    app_id: String,
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
            let mut resources = HashMap::new();
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
                            let resource = match resources.entry(resource_id) {
                                std::collections::hash_map::Entry::Occupied(_) => None,
                                std::collections::hash_map::Entry::Vacant(entry) => {
                                    match dmabuf_resource(&target, resource_id) {
                                        Ok(resource) => {
                                            entry.insert(resource.app_id.clone());
                                            Some(resource)
                                        }
                                        Err(error) => {
                                            let _send_result = proxy.send_event(
                                                ClientEvent::TransportError(format!(
                                                    "fetch DMA-BUF resource {resource_id}: {error:#}"
                                                )),
                                            );
                                            return;
                                        }
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
                                app_id: resources.get(&resource_id).cloned().unwrap_or_default(),
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

struct BackendCommandLane {
    sender: Option<mpsc::SyncSender<BackendCommand>>,
    worker: Option<thread::JoinHandle<()>>,
}

struct BackendCommand {
    description: String,
    argv: Vec<String>,
}

impl BackendCommandLane {
    fn start(
        target: ControlTarget,
        proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<BackendCommand>(32);
        let worker = thread::Builder::new()
            .name("manifold-backend-commands".to_string())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    let result = request_dbus_command(&target, &command.argv, false)
                        .map_err(|error| format!("{error:#}"));
                    if proxy
                        .send_event(ClientEvent::BackendCommandCompleted {
                            description: command.description,
                            result,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .context("spawn presentation backend command worker")?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    fn try_send(&self, description: impl Into<String>, argv: Vec<String>) -> Result<()> {
        self.sender
            .as_ref()
            .context("presentation backend command worker is stopped")?
            .try_send(BackendCommand {
                description: description.into(),
                argv,
            })
            .context("presentation backend command queue is full or closed")
    }

    fn shutdown(&mut self) -> Result<()> {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_panic| anyhow!("presentation backend command worker panicked"))?;
        }
        Ok(())
    }
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
    window_focused: bool,
    cursor_position: Option<(f64, f64)>,
    pending_plugin_pointer_motion: Option<(f32, f32)>,
    wasm_runtime: Option<PresentationWasmRuntime>,
    layout_policy: Option<ClientLayoutPolicy>,
    key_policy: Option<ClientKeyPolicy>,
    plugin_graphics: Option<PluginGraphicsBridge>,
    backend_commands: Option<BackendCommandLane>,
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
            ClientEvent::BackendCommandCompleted { .. } => {
                return Err(anyhow!(
                    "backend command completion reached scene transport path"
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
        let focus_change = renderer.select_scene(sequence, width, height, windows)?;
        if let Some(change) = focus_change {
            dispatch_focus_change(
                self.wasm_runtime.as_ref(),
                self.input_sender.as_mut(),
                change,
                self.window_focused,
            )?;
        }
        flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
        Ok(())
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
        let mut old = self
            .renderer
            .take()
            .context("lost renderer during recovery")?;
        let replay = old
            .plugin_pane
            .as_ref()
            .map(|plugin| (plugin.scene.pane, plugin.scene.sequence));
        let window = Arc::clone(&old.window);
        let layout_policy = old
            .layout_policy
            .take()
            .context("lost layout policy during renderer recovery")?;
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
        let mut replacement = Renderer::new(
            window,
            self.fast_path_enabled,
            graphics,
            layout_policy,
            generation,
            proxy,
        )?;
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
        let Some(layout_policy) = self.layout_policy.take() else {
            self.fail(
                event_loop,
                anyhow!("presentation layout policy is unavailable"),
            );
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
                    layout_policy,
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
                let mut failure = None;
                if let Some(renderer) = self.renderer.as_mut() {
                    match renderer.refresh_plugin_graphics(pane, sequence) {
                        Ok(Some(change)) => {
                            if let Err(error) = dispatch_focus_change(
                                self.wasm_runtime.as_ref(),
                                self.input_sender.as_mut(),
                                change,
                                self.window_focused,
                            ) {
                                failure = Some(error);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => failure = Some(error),
                    }
                    flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
                }
                if let Some(error) = failure {
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
            ClientEvent::BackendCommandCompleted {
                description,
                result,
            } => match result {
                Ok(response) => eprintln!(
                    "presentation backend command completed command={description} response={}",
                    response.trim()
                ),
                Err(error) => {
                    eprintln!("presentation backend command failed command={description}: {error}");
                }
            },
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
                let focus_change = match renderer.resize(size.width, size.height) {
                    Ok(change) => change,
                    Err(error) => {
                        self.fail(event_loop, error);
                        return;
                    }
                };
                if let Some(change) = focus_change
                    && let Err(error) = dispatch_focus_change(
                        self.wasm_runtime.as_ref(),
                        self.input_sender.as_mut(),
                        change,
                        self.window_focused,
                    )
                {
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
                flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = Some((position.x, position.y));
                if renderer.focused_plugin_pane().is_some() {
                    self.pending_plugin_pointer_motion =
                        renderer.plugin_position_for_window_point(position.x, position.y);
                } else if renderer.focused_managed_window().is_some() {
                    self.pending_plugin_pointer_motion = None;
                    if let (Some(sender), Some((x, y))) = (
                        self.input_sender.as_mut(),
                        renderer.backend_position_for_window_point(position.x, position.y),
                    ) {
                        send_input(
                            sender,
                            manifold_ipc::INPUT_POINTER_MOTION,
                            [x.to_bits(), y.to_bits(), 0, 0, 0],
                        );
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed
                    && button == MouseButton::Left
                    && let Some((x, y)) = self.cursor_position
                {
                    if let Some(change) = renderer.focus_pane_at_window_point(x, y)
                        && let Err(error) = dispatch_focus_change(
                            self.wasm_runtime.as_ref(),
                            self.input_sender.as_mut(),
                            change,
                            self.window_focused,
                        )
                    {
                        eprintln!("dropped pane focus event: {error:#}");
                    }
                    flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
                }

                let button_code = linux_button_code(button).unwrap_or_default();
                let pressed = state == ElementState::Pressed;
                let modifiers = modifier_bits(self.modifiers);
                if let Some(pane) = renderer.focused_plugin_pane()
                    && let Some((x, y)) = self
                        .cursor_position
                        .and_then(|(x, y)| renderer.plugin_position_for_window_point(x, y))
                {
                    if let Err(error) = send_presentation_event(
                        self.wasm_runtime.as_ref(),
                        PresentationEvent::PointerButton {
                            pane,
                            x,
                            y,
                            button: button_code,
                            pressed,
                            modifiers,
                        },
                    ) {
                        eprintln!("dropped plugin pointer button: {error:#}");
                    }
                } else if renderer.focused_managed_window().is_some()
                    && let (Some(sender), Some(code)) =
                        (self.input_sender.as_mut(), linux_button_code(button))
                {
                    if let Some((x, y)) = self
                        .cursor_position
                        .and_then(|(x, y)| renderer.backend_position_for_window_point(x, y))
                    {
                        send_input(
                            sender,
                            manifold_ipc::INPUT_POINTER_MOTION,
                            [x.to_bits(), y.to_bits(), 0, 0, 0],
                        );
                    }
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
                if let Some(pane) = renderer.focused_plugin_pane()
                    && let Some((x, y)) = self
                        .cursor_position
                        .and_then(|(x, y)| renderer.plugin_position_for_window_point(x, y))
                {
                    let scale = renderer.window.scale_factor() as f32;
                    if let Err(error) = send_presentation_event(
                        self.wasm_runtime.as_ref(),
                        PresentationEvent::PointerScroll {
                            pane,
                            x,
                            y,
                            horizontal: horizontal as f32 / scale,
                            vertical: vertical as f32 / scale,
                            modifiers: modifier_bits(self.modifiers),
                        },
                    ) {
                        eprintln!("dropped plugin pointer scroll: {error:#}");
                    }
                } else if renderer.focused_managed_window().is_some()
                    && let Some(sender) = self.input_sender.as_mut()
                {
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
                self.window_focused = focused;
                if let Some(pane) = renderer.focused_plugin_pane()
                    && let Err(error) = send_presentation_event(
                        self.wasm_runtime.as_ref(),
                        PresentationEvent::PaneFocused { pane, focused },
                    )
                {
                    eprintln!("dropped plugin outer-window focus event: {error:#}");
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } if !is_synthetic => {
                // TEMPORARY MVP BINDING: F6 toggles between the one local plugin pane
                // and the backend-managed pane without carrying modifiers across them.
                if is_plugin_managed_focus_toggle(event.physical_key) {
                    if event.state == ElementState::Pressed && !event.repeat {
                        if let Some(change) = renderer.toggle_plugin_managed_focus()
                            && let Err(error) = dispatch_focus_change(
                                self.wasm_runtime.as_ref(),
                                self.input_sender.as_mut(),
                                change,
                                self.window_focused,
                            )
                        {
                            eprintln!("dropped pane focus event: {error:#}");
                        }
                        flush_renderer_managed_resize_requests(
                            self.input_sender.as_mut(),
                            renderer,
                        );
                    }
                    return;
                }

                let policy_evaluation = event.physical_key.to_scancode().and_then(|scancode| {
                    let snapshot = renderer.key_policy_snapshot();
                    self.key_policy.as_mut().and_then(|policy| {
                        match policy.evaluate(
                            i32::try_from(scancode.saturating_add(8)).unwrap_or(i32::MAX),
                            match event.state {
                                ElementState::Pressed => manifold_wasm::ABI_KEY_PRESSED,
                                ElementState::Released => manifold_wasm::ABI_KEY_RELEASED,
                            },
                            key_policy_modifier_bits(self.modifiers),
                            &snapshot,
                        ) {
                            Ok(evaluation) => Some(evaluation),
                            Err(error) => {
                                eprintln!(
                                    "presentation key policy failed; forwarding key: {error:#}"
                                );
                                None
                            }
                        }
                    })
                });

                let mut forward_key = true;
                if let Some(evaluation) = policy_evaluation {
                    forward_key = matches!(
                        evaluation.decision,
                        GlobalKeyDecision::Abstain
                            | GlobalKeyDecision::Forward
                            | GlobalKeyDecision::ForwardWithModifiers
                    );
                    for message in evaluation.actor_messages {
                        if let Some(runtime) = self.wasm_runtime.as_ref()
                            && let Err(error) =
                                runtime.send_actor_message(&message.actor, message.payload)
                        {
                            eprintln!("dropped key-policy actor message: {error:#}");
                        }
                    }
                    for message in evaluation.host_messages {
                        if let Err(error) = apply_key_policy_host_message(
                            renderer,
                            &mut self.input_sender,
                            self.backend_commands.as_ref(),
                            self.wasm_runtime.as_ref(),
                            message,
                            self.window_focused,
                        ) {
                            eprintln!("rejected key-policy host message: {error:#}");
                        }
                    }
                    flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
                }

                if !forward_key {
                    return;
                }

                let modifiers = modifier_bits(self.modifiers);
                if let Some(pane) = renderer.focused_plugin_pane() {
                    if let Some(physical_key) = event.physical_key.to_scancode()
                        && let Err(error) = send_presentation_event(
                            self.wasm_runtime.as_ref(),
                            PresentationEvent::Key {
                                pane,
                                physical_key,
                                pressed: event.state == ElementState::Pressed,
                                repeat: event.repeat,
                                modifiers,
                            },
                        )
                    {
                        eprintln!("dropped plugin key event: {error:#}");
                    }
                    if event.state == ElementState::Pressed
                        && let Some(text) = event.text.as_ref()
                        && !text.is_empty()
                        && let Err(error) = send_presentation_event(
                            self.wasm_runtime.as_ref(),
                            PresentationEvent::Text {
                                pane,
                                text: text.to_string(),
                            },
                        )
                    {
                        eprintln!("dropped plugin text event: {error:#}");
                    }
                } else if renderer.focused_managed_window().is_some()
                    && let (Some(sender), Some(scancode)) =
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
    current_sequence: Option<u64>,
    plugin_graphics: PluginGraphicsBridge,
    plugin_pane: Option<PluginPaneTexture>,
    focused_pane: Option<FocusedPane>,
    layout_policy: Option<ClientLayoutPolicy>,
    accepted_layout: ResolvedLayout,
    layout_source: LayoutSource,
    fullscreen_pane: Option<u64>,
    sent_managed_sizes: HashMap<u64, (u32, u32)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusedPane {
    Plugin(u64),
    Managed(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FocusChange {
    plugin: Option<(u64, bool)>,
    managed_window: Option<u64>,
}

struct SceneWindow {
    window_id: u64,
    app_id: String,
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
    rect: [i32; 4],
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

fn flush_renderer_managed_resize_requests(lane: Option<&mut InputLane>, renderer: &mut Renderer) {
    let Some(lane) = lane else {
        return;
    };
    for (window, width, height) in renderer.pending_managed_resize_requests() {
        if send_input(
            lane,
            manifold_ipc::INPUT_WINDOW_RESIZE,
            [window, u64::from(width), u64::from(height), 0, 0],
        ) {
            renderer.mark_managed_resize_sent(window, width, height);
            eprintln!(
                "requested managed window resize window_id={window} logical_size={width}x{height}"
            );
        }
    }
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

fn send_presentation_event(
    runtime: Option<&PresentationWasmRuntime>,
    event: PresentationEvent,
) -> Result<()> {
    let Some(runtime) = runtime else {
        return Ok(());
    };
    runtime.send_presentation_event(event)
}

fn dispatch_focus_change(
    runtime: Option<&PresentationWasmRuntime>,
    input_sender: Option<&mut InputLane>,
    change: FocusChange,
    window_focused: bool,
) -> Result<()> {
    if let Some((pane, focused)) = change.plugin {
        send_presentation_event(
            runtime,
            PresentationEvent::PaneFocused {
                pane,
                focused: focused && window_focused,
            },
        )?;
        eprintln!(
            "focused {} plugin pane={pane}",
            if focused { "local" } else { "away from" }
        );
    }
    if let Some(window_id) = change.managed_window {
        if let Some(sender) = input_sender {
            send_window_focus(sender, window_id);
        }
        eprintln!("focused backend-managed window={window_id}");
    }
    Ok(())
}

fn apply_key_policy_host_message(
    renderer: &mut Renderer,
    input_sender: &mut Option<InputLane>,
    backend_commands: Option<&BackendCommandLane>,
    runtime: Option<&PresentationWasmRuntime>,
    message: HostMessage,
    window_focused: bool,
) -> Result<()> {
    let focus_change = match message {
        HostMessage::LayoutAction { action } => {
            let action = action
                .parse::<LayoutAction>()
                .map_err(|error| anyhow!("parse key-policy layout action: {error}"))?;
            match action {
                LayoutAction::FocusNext | LayoutAction::FocusNextSplit => renderer.cycle_focus(1),
                LayoutAction::FocusPrevious | LayoutAction::FocusPreviousSplit => {
                    renderer.cycle_focus(-1)
                }
                LayoutAction::CloseActivePane => {
                    if let Some(pane) = renderer.focused_managed_window() {
                        queue_backend_command(
                            backend_commands,
                            format!("close-pane-{pane}"),
                            vec!["close-pane".to_string(), pane.to_string()],
                        )?;
                    } else {
                        eprintln!("key-policy close ignored for a presentation-local actor pane");
                    }
                    None
                }
                LayoutAction::ToggleManagedFullscreen => {
                    if renderer.focused_managed_window().is_some() {
                        renderer.toggle_focused_fullscreen()
                    } else {
                        None
                    }
                }
                LayoutAction::SplitActiveRight
                | LayoutAction::SplitActiveDown
                | LayoutAction::JoinActivePane
                | LayoutAction::SplitActiveSmart
                | LayoutAction::MoveActiveToLeftDock
                | LayoutAction::MoveActiveToRightDock
                | LayoutAction::MoveActiveToBottomDock
                | LayoutAction::MoveActiveToCenter
                | LayoutAction::ToggleLeftDock
                | LayoutAction::ToggleRightDock
                | LayoutAction::ToggleBottomDock
                | LayoutAction::IncreaseLeftDockWidth
                | LayoutAction::DecreaseLeftDockWidth => {
                    eprintln!(
                        "key-policy layout action action={action} has no stateful operation in the selected flat layout provider"
                    );
                    None
                }
            }
        }
        HostMessage::FocusPane { pane } => renderer.focus_pane(pane),
        HostMessage::SpawnApp { argv } => {
            queue_spawn_command(backend_commands, argv)?;
            None
        }
        HostMessage::FocusOrSpawnApp { app_id, argv } => {
            if let Some(pane) = renderer.pane_for_app_id(&app_id) {
                renderer.focus_pane(pane)
            } else {
                queue_spawn_command(backend_commands, argv)?;
                None
            }
        }
        HostMessage::SwitchActiveSessionNext => {
            queue_backend_command(
                backend_commands,
                "switch-active-session-next",
                vec!["surface-session".to_string(), "next".to_string()],
            )?;
            None
        }
        HostMessage::CreateAndAttachSession => {
            queue_backend_command(
                backend_commands,
                "create-and-attach-session",
                vec!["surface-session".to_string(), "new".to_string()],
            )?;
            None
        }
        HostMessage::ToggleShortcutPassthrough | HostMessage::SetShortcutPassthrough { .. } => None,
        HostMessage::SetResolvedLayout { .. } => {
            return Err(anyhow!(
                "key policy cannot replace the selected client layout provider"
            ));
        }
    };

    if let Some(change) = focus_change {
        dispatch_focus_change(runtime, input_sender.as_mut(), change, window_focused)?;
    }
    Ok(())
}

fn queue_spawn_command(
    backend_commands: Option<&BackendCommandLane>,
    argv: Vec<String>,
) -> Result<()> {
    let program = argv
        .first()
        .context("key-policy spawn argv is empty")?
        .clone();
    let mut command = Vec::with_capacity(argv.len() + 1);
    command.push("spawn".to_string());
    command.extend(argv);
    queue_backend_command(backend_commands, format!("spawn-{program}"), command)
}

fn queue_backend_command(
    backend_commands: Option<&BackendCommandLane>,
    description: impl Into<String>,
    argv: Vec<String>,
) -> Result<()> {
    backend_commands
        .context("key-policy command requires a connected backend")?
        .try_send(description, argv)
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

fn key_policy_modifier_bits(modifiers: ModifiersState) -> i32 {
    (i32::from(modifiers.control_key()) * manifold_wasm::ABI_MOD_CTRL)
        | (i32::from(modifiers.alt_key()) * manifold_wasm::ABI_MOD_ALT)
        | (i32::from(modifiers.shift_key()) * manifold_wasm::ABI_MOD_SHIFT)
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
            "imported window DMA-BUF window_id={} app_id={} resource_id={} size={}x{} format={:?} color_space={:?} stride={} modifier={} y_inverted={}",
            window_id,
            frame.app_id,
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
        layout_policy: ClientLayoutPolicy,
        generation: u64,
        proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
    ) -> Result<Self> {
        pollster::block_on(Self::new_async(
            window,
            fast_path_enabled,
            plugin_graphics,
            layout_policy,
            generation,
            proxy,
        ))
    }

    async fn new_async(
        window: Arc<Window>,
        fast_path_enabled: bool,
        plugin_graphics: PluginGraphicsBridge,
        layout_policy: ClientLayoutPolicy,
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
            current_sequence: None,
            plugin_graphics,
            plugin_pane: None,
            focused_pane: None,
            layout_policy: Some(layout_policy),
            accepted_layout: ResolvedLayout::empty(),
            layout_source: LayoutSource::NativeFallback,
            fullscreen_pane: None,
            sent_managed_sizes: HashMap::new(),
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

    fn select_scene(
        &mut self,
        sequence: u64,
        _width: u32,
        _height: u32,
        windows: Vec<SceneWindowUpdate>,
    ) -> Result<Option<FocusChange>> {
        let mut scene_windows = Vec::with_capacity(windows.len());
        let mut backend_focus = None;
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
            if window.active {
                backend_focus = Some(window.window_id);
            }
            scene_windows.push(SceneWindow {
                window_id: window.window_id,
                app_id: window.app_id,
                resource_id: window.resource_id,
                source_rect: window.rect,
                rect: window.rect,
                active: window.active,
            });
        }
        scene_windows.sort_by_key(|window| window.window_id);
        if scene_windows
            .iter()
            .all(|window| self.imported_frames.contains_key(&window.resource_id))
        {
            self.current_windows = scene_windows;
            self.current_sequence = Some(sequence);
            self.focused_pane = match self.focused_pane {
                Some(FocusedPane::Plugin(pane))
                    if self
                        .plugin_pane
                        .as_ref()
                        .is_some_and(|plugin| plugin.scene.pane == pane) =>
                {
                    Some(FocusedPane::Plugin(pane))
                }
                Some(FocusedPane::Managed(window_id))
                    if self
                        .current_windows
                        .iter()
                        .any(|window| window.window_id == window_id) =>
                {
                    Some(FocusedPane::Managed(window_id))
                }
                _ => backend_focus
                    .or_else(|| self.current_windows.first().map(|window| window.window_id))
                    .map(FocusedPane::Managed)
                    .or_else(|| {
                        self.plugin_pane
                            .as_ref()
                            .map(|plugin| FocusedPane::Plugin(plugin.scene.pane))
                    }),
            };
            let focus_change = self.relayout_panes();
            self.window.request_redraw();
            return Ok(focus_change);
        }
        Ok(None)
    }

    const fn focused_plugin_pane(&self) -> Option<u64> {
        match self.focused_pane {
            Some(FocusedPane::Plugin(pane)) => Some(pane),
            _ => None,
        }
    }

    const fn focused_managed_window(&self) -> Option<u64> {
        match self.focused_pane {
            Some(FocusedPane::Managed(window_id)) => Some(window_id),
            _ => None,
        }
    }

    fn toggle_plugin_managed_focus(&mut self) -> Option<FocusChange> {
        let plugin = self.plugin_pane.as_ref().map(|pane| pane.scene.pane)?;
        let managed = match self.focused_pane {
            Some(FocusedPane::Managed(window_id))
                if self
                    .current_windows
                    .iter()
                    .any(|window| window.window_id == window_id) =>
            {
                window_id
            }
            _ => {
                let window = self.current_windows.first()?;
                window.window_id
            }
        };
        let target = toggled_plugin_managed_focus(self.focused_pane, plugin, managed);
        self.set_focus(target)
    }

    fn cycle_focus(&mut self, offset: isize) -> Option<FocusChange> {
        let panes = self.pane_inventory();
        if panes.is_empty() {
            return None;
        }
        let current = self
            .focused_pane_id()
            .and_then(|focused| panes.iter().position(|pane| *pane == focused))
            .unwrap_or(0);
        let len = isize::try_from(panes.len()).unwrap_or(isize::MAX);
        let target_index = (isize::try_from(current).unwrap_or_default() + offset).rem_euclid(len);
        let target_pane = *panes.get(usize::try_from(target_index).ok()?)?;
        let target = self.focus_target_for_pane(target_pane)?;
        if self.fullscreen_pane.is_some() {
            self.fullscreen_pane = Some(target_pane);
        }
        self.set_focus(target)
    }

    fn focus_pane(&mut self, pane: u64) -> Option<FocusChange> {
        let target = self.focus_target_for_pane(pane)?;
        if self.fullscreen_pane.is_some() {
            self.fullscreen_pane = Some(pane);
        }
        self.set_focus(target)
    }

    fn toggle_focused_fullscreen(&mut self) -> Option<FocusChange> {
        let focused = self.focused_pane_id()?;
        self.fullscreen_pane = (self.fullscreen_pane != Some(focused)).then_some(focused);
        let focus_change = self.relayout_panes();
        self.window.request_redraw();
        focus_change
    }

    fn pane_for_app_id(&self, app_id: &str) -> Option<u64> {
        self.current_windows
            .iter()
            .find_map(|window| (window.app_id == app_id).then_some(window.window_id))
    }

    fn key_policy_snapshot(&self) -> KeyPolicySnapshot {
        KeyPolicySnapshot {
            focused_pane: self.focused_pane_id(),
            panes: self.pane_inventory(),
            app_panes: self
                .current_windows
                .iter()
                .filter(|window| !window.app_id.is_empty())
                .map(|window| KeyPolicyAppPane {
                    pane: window.window_id,
                    app_id: window.app_id.clone(),
                })
                .collect(),
        }
    }

    fn focus_pane_at_window_point(&mut self, x: f64, y: f64) -> Option<FocusChange> {
        let (x, y) = self.scene_position_for_window_point(x, y);
        let (width, height) = self.composition_extent();
        let bounds = LayoutRect {
            x: 0,
            y: 0,
            w: i32::try_from(width).unwrap_or(i32::MAX).max(1),
            h: i32::try_from(height).unwrap_or(i32::MAX).max(1),
        };
        let hit = self
            .accepted_layout
            .hit_stack(LocalPoint::from_pixel_in_rect(x, y, bounds))
            .into_iter()
            .next()?;
        let LayoutItemKind::Pane { pane } = hit.kind else {
            return None;
        };
        let target = self.focus_target_for_pane(pane)?;
        self.set_focus(target)
    }

    fn set_focus(&mut self, target: FocusedPane) -> Option<FocusChange> {
        let previous = self.focused_pane;
        if previous == Some(target) {
            return None;
        }
        self.focused_pane = Some(target);
        let _focus_change = self.relayout_panes();
        let target = self.focused_pane?;
        if previous == Some(target) {
            return None;
        }
        for window in &mut self.current_windows {
            window.active = matches!(target, FocusedPane::Managed(id) if id == window.window_id);
        }
        self.window.request_redraw();
        focus_change_between(previous, Some(target))
    }

    fn scene_position_for_window_point(&self, x: f64, y: f64) -> (f64, f64) {
        let (content_width, content_height) = self.composition_extent();
        let window_size = self.window.inner_size();
        (
            x * f64::from(content_width) / f64::from(window_size.width.max(1)),
            y * f64::from(content_height) / f64::from(window_size.height.max(1)),
        )
    }

    fn backend_position_for_window_point(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (x, y) = self.scene_position_for_window_point(x, y);
        let window = self.current_windows.iter().rev().find(|window| {
            self.focused_managed_window() == Some(window.window_id)
                && rect_contains(window.rect, x, y)
        })?;
        Some(project_point_between_rects(
            x,
            y,
            window.rect,
            window.source_rect,
        ))
    }

    fn plugin_position_for_window_point(&self, x: f64, y: f64) -> Option<(f32, f32)> {
        let plugin = self.plugin_pane.as_ref()?;
        if self.focused_plugin_pane() != Some(plugin.scene.pane) {
            return None;
        }
        let (x, y) = self.scene_position_for_window_point(x, y);
        if !rect_contains(plugin.rect, x, y) {
            return None;
        }
        let (composition_width, composition_height) = self.composition_extent();
        let (x, y) = project_point_between_rects(
            x,
            y,
            plugin.rect,
            [
                0,
                0,
                i32::try_from(composition_width).unwrap_or(i32::MAX),
                i32::try_from(composition_height).unwrap_or(i32::MAX),
            ],
        );
        let scale = self.window.scale_factor() as f32;
        Some((x as f32 / scale, y as f32 / scale))
    }

    fn composition_extent(&self) -> (u32, u32) {
        (self.config.width.max(1), self.config.height.max(1))
    }

    fn relayout_panes(&mut self) -> Option<FocusChange> {
        let previous_focus = self.focused_pane;
        let (width, height) = self.composition_extent();
        let extent = LayoutSize {
            w: i32::try_from(width).unwrap_or(i32::MAX).max(1),
            h: i32::try_from(height).unwrap_or(i32::MAX).max(1),
        };
        let panes = self.pane_inventory();
        let focused = self.focused_pane_id();
        let layout_policy = self.layout_policy.as_mut()?;
        let resolution = layout_policy.resolve(&panes, focused, extent);
        if self.layout_source != resolution.source {
            let source = match resolution.source {
                LayoutSource::NativeFallback => "native-fallback",
                LayoutSource::WasmPolicy => "wasm-policy",
            };
            eprintln!("presentation layout source changed source={source}");
        }
        self.layout_source = resolution.source;
        self.accepted_layout = resolution.layout;
        if let Some(pane) = self.fullscreen_pane.filter(|pane| panes.contains(pane)) {
            self.accepted_layout = fullscreen_layout(pane);
        }

        if let Some(plugin) = self.plugin_pane.as_mut() {
            plugin.rect = [0, 0, 0, 0];
        }
        for window in &mut self.current_windows {
            window.rect = [0, 0, 0, 0];
        }
        for item in &self.accepted_layout.items {
            let LayoutItemKind::Pane { pane } = item.kind else {
                continue;
            };
            let rect = item.pixel_rect(extent);
            let rect = [rect.x, rect.y, rect.w, rect.h];
            if let Some(plugin) = self
                .plugin_pane
                .as_mut()
                .filter(|plugin| plugin.scene.pane == pane)
            {
                plugin.rect = rect;
            } else if let Some(window) = self
                .current_windows
                .iter_mut()
                .find(|window| window.window_id == pane)
            {
                window.rect = rect;
            }
        }

        let requested_focus = resolution
            .focus
            .and_then(|pane| self.focus_target_for_visible_pane(pane));
        let retained_focus = self
            .focused_pane_id()
            .and_then(|pane| self.focus_target_for_visible_pane(pane));
        let fallback_focus = self
            .accepted_layout
            .items
            .iter()
            .find_map(|item| match item.kind {
                LayoutItemKind::Pane { pane } => Some(pane),
                LayoutItemKind::Placeholder
                | LayoutItemKind::Handle
                | LayoutItemKind::Chrome
                | LayoutItemKind::Overlay => None,
            })
            .and_then(|pane| self.focus_target_for_pane(pane));
        self.focused_pane = requested_focus.or(retained_focus).or(fallback_focus);
        for window in &mut self.current_windows {
            window.active = self.focused_pane == Some(FocusedPane::Managed(window.window_id));
        }
        self.sent_managed_sizes.retain(|window, _| {
            self.current_windows
                .iter()
                .any(|item| item.window_id == *window)
        });
        self.fullscreen_pane = self.fullscreen_pane.filter(|pane| panes.contains(pane));
        focus_change_between(previous_focus, self.focused_pane)
    }

    fn pane_inventory(&self) -> Vec<u64> {
        self.plugin_pane
            .iter()
            .map(|plugin| plugin.scene.pane)
            .chain(self.current_windows.iter().map(|window| window.window_id))
            .collect()
    }

    const fn focused_pane_id(&self) -> Option<u64> {
        match self.focused_pane {
            Some(FocusedPane::Plugin(pane) | FocusedPane::Managed(pane)) => Some(pane),
            None => None,
        }
    }

    fn focus_target_for_pane(&self, pane: u64) -> Option<FocusedPane> {
        if self
            .plugin_pane
            .as_ref()
            .is_some_and(|plugin| plugin.scene.pane == pane)
        {
            return Some(FocusedPane::Plugin(pane));
        }
        self.current_windows
            .iter()
            .any(|window| window.window_id == pane)
            .then_some(FocusedPane::Managed(pane))
    }

    fn focus_target_for_visible_pane(&self, pane: u64) -> Option<FocusedPane> {
        self.accepted_layout
            .pane_item(pane)
            .and_then(|_| self.focus_target_for_pane(pane))
    }

    fn refresh_plugin_graphics(
        &mut self,
        pane: u64,
        expected_sequence: u64,
    ) -> Result<Option<FocusChange>> {
        let Some(snapshot) = self.plugin_graphics.snapshot(pane)? else {
            return Ok(None);
        };
        if snapshot.sequence < expected_sequence {
            return Ok(None);
        }
        if self.plugin_pane.as_ref().is_some_and(|current| {
            current.scene.pane == snapshot.pane
                && current.scene.sequence >= snapshot.sequence
                && current.scene.width == self.config.width
                && current.scene.height == self.config.height
        }) {
            return Ok(None);
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
        let first_plugin_scene = self.plugin_pane.is_none();
        self.plugin_pane = Some(PluginPaneTexture {
            scene,
            rect: [
                0,
                0,
                i32::try_from(self.config.width).unwrap_or(i32::MAX),
                i32::try_from(self.config.height).unwrap_or(i32::MAX),
            ],
        });
        let focus_change = if first_plugin_scene {
            self.set_focus(FocusedPane::Plugin(pane))
        } else {
            self.relayout_panes()
        };
        self.window.request_redraw();
        Ok(focus_change)
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<Option<FocusChange>> {
        if width == 0 || height == 0 {
            return Ok(None);
        }

        self.config.width = width;
        self.config.height = height;
        self.reconfigure();
        let focus_change = if let Some((pane, sequence)) = self
            .plugin_pane
            .as_ref()
            .map(|plugin| (plugin.scene.pane, plugin.scene.sequence))
        {
            self.refresh_plugin_graphics(pane, sequence)?
        } else {
            self.relayout_panes()
        };
        self.window.request_redraw();
        Ok(focus_change)
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
            let extent = self.composition_extent();
            for pane in self.layout_panes_in_render_order() {
                if let Some(plugin) = self
                    .plugin_pane
                    .as_ref()
                    .filter(|plugin| plugin.scene.pane == pane)
                {
                    let Some(rect) = clip_rect_to_extent(plugin.rect, extent) else {
                        continue;
                    };
                    let viewport =
                        scaled_viewport(rect, extent, (self.config.width, self.config.height));
                    render_pass.set_viewport(
                        viewport[0],
                        viewport[1],
                        viewport[2],
                        viewport[3],
                        0.0,
                        1.0,
                    );
                    render_pass.set_bind_group(0, plugin.scene.output_bind_group(), &[]);
                    render_pass.draw(0..3, 0..1);
                    continue;
                }

                let Some(window) = self
                    .current_windows
                    .iter()
                    .find(|window| window.window_id == pane)
                else {
                    continue;
                };
                let Some(imported) = self.imported_frames.get(&window.resource_id) else {
                    continue;
                };
                let Some(rect) = clip_rect_to_extent(window.rect, extent) else {
                    continue;
                };
                let viewport =
                    scaled_viewport(rect, extent, (self.config.width, self.config.height));
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

    fn layout_panes_in_render_order(&self) -> Vec<u64> {
        let mut panes = self
            .accepted_layout
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item.kind {
                LayoutItemKind::Pane { pane } => Some((item.z, index, pane)),
                LayoutItemKind::Placeholder
                | LayoutItemKind::Handle
                | LayoutItemKind::Chrome
                | LayoutItemKind::Overlay => None,
            })
            .collect::<Vec<_>>();
        panes.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        panes.into_iter().map(|(_, _, pane)| pane).collect()
    }

    fn pending_managed_resize_requests(&self) -> Vec<(u64, u32, u32)> {
        let scale = self.window.scale_factor().max(f64::EPSILON);
        self.current_windows
            .iter()
            .filter_map(|window| {
                let [_, _, width, height] = window.rect;
                if width <= 0 || height <= 0 {
                    return None;
                }
                let width = ((f64::from(width) / scale).round() as u32).max(1);
                let height = ((f64::from(height) / scale).round() as u32).max(1);
                (self.sent_managed_sizes.get(&window.window_id) != Some(&(width, height)))
                    .then_some((window.window_id, width, height))
            })
            .collect()
    }

    fn mark_managed_resize_sent(&mut self, window: u64, width: u32, height: u32) {
        self.sent_managed_sizes.insert(window, (width, height));
    }
}

fn clip_rect_to_extent(rect: [i32; 4], extent: (u32, u32)) -> Option<[i32; 4]> {
    let extent_width = i32::try_from(extent.0).unwrap_or(i32::MAX);
    let extent_height = i32::try_from(extent.1).unwrap_or(i32::MAX);
    let [x, y, width, height] = rect;
    let x = x.clamp(0, extent_width);
    let y = y.clamp(0, extent_height);
    let width = width.max(0).min(extent_width - x);
    let height = height.max(0).min(extent_height - y);
    (width > 0 && height > 0).then_some([x, y, width, height])
}

const fn toggled_plugin_managed_focus(
    current: Option<FocusedPane>,
    plugin: u64,
    managed: u64,
) -> FocusedPane {
    match current {
        Some(FocusedPane::Plugin(_)) => FocusedPane::Managed(managed),
        _ => FocusedPane::Plugin(plugin),
    }
}

fn focus_change_between(
    previous: Option<FocusedPane>,
    target: Option<FocusedPane>,
) -> Option<FocusChange> {
    if matches!((previous, target), (None, None)) || previous == target {
        return None;
    }
    let plugin = match (previous, target) {
        (Some(FocusedPane::Plugin(pane)), Some(FocusedPane::Managed(_)) | None) => {
            Some((pane, false))
        }
        (_, Some(FocusedPane::Plugin(pane))) => Some((pane, true)),
        _ => None,
    };
    let managed_window = match target {
        Some(FocusedPane::Managed(window_id)) => Some(window_id),
        Some(FocusedPane::Plugin(_)) | None => None,
    };
    (plugin.is_some() || managed_window.is_some()).then_some(FocusChange {
        plugin,
        managed_window,
    })
}

const fn is_plugin_managed_focus_toggle(key: PhysicalKey) -> bool {
    matches!(key, PhysicalKey::Code(KeyCode::F6))
}

fn fullscreen_layout(pane: u64) -> ResolvedLayout {
    ResolvedLayout {
        items: vec![ResolvedLayoutItem {
            id: LayoutItemId(pane),
            kind: LayoutItemKind::Pane { pane },
            z: 0.0,
            transform: LocalTransform {
                offset: LocalPoint { u: -1.0, v: -1.0 },
                size: LocalPoint { u: 2.0, v: 2.0 },
            },
            input: InputBehavior::Pane,
        }],
    }
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
    fn flat_layout_viewports_are_clipped_to_the_client_surface() {
        assert_eq!(
            clip_rect_to_extent([-20, 10, 200, 100], (120, 80)),
            Some([0, 10, 120, 70])
        );
        assert_eq!(clip_rect_to_extent([200, 10, 20, 20], (120, 80)), None);
    }

    #[test]
    fn focus_toggle_distinguishes_local_and_backend_ids() {
        assert!(is_plugin_managed_focus_toggle(PhysicalKey::Code(
            KeyCode::F6
        )));
        assert!(!is_plugin_managed_focus_toggle(PhysicalKey::Code(
            KeyCode::F5
        )));
        assert_eq!(
            toggled_plugin_managed_focus(Some(FocusedPane::Plugin(1)), 1, 1),
            FocusedPane::Managed(1)
        );
        assert_eq!(
            toggled_plugin_managed_focus(Some(FocusedPane::Managed(1)), 1, 1),
            FocusedPane::Plugin(1)
        );
    }

    #[test]
    fn client_pointer_position_projects_into_the_matching_backend_window() {
        let actual =
            project_point_between_rects(900.0, 200.0, [600, 0, 600, 400], [0, 400, 600, 400]);
        assert_eq!(actual, (300.0, 600.0));
    }
}
