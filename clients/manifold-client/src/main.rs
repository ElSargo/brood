use std::{
    collections::{HashMap, HashSet},
    os::fd::AsFd,
    sync::Arc,
    thread,
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
use wgpu::{Color, CurrentSurfaceTexture};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    platform::scancode::PhysicalKeyExtScancode,
    window::{Window, WindowId},
};

fn main() -> Result<()> {
    let args = ClientArgs::parse();
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
    let event_loop = EventLoop::<ClientEvent>::with_user_event()
        .build()
        .context("create event loop")?;
    let input_sender = if fast_path_enabled {
        let fast_path = open_presentation_fast_path(&target)?;
        let input =
            manifold_ipc::CriticalProducer::map(fast_path.input_fd.as_fd(), fast_path.generation)?;
        let frames =
            manifold_ipc::CriticalConsumer::map(fast_path.frame_fd.as_fd(), fast_path.generation)?;
        spawn_frame_updates(target, frames, event_loop.create_proxy())?;
        Some(InputLane {
            producer: input,
            generation: fast_path.generation,
            sequence: 0,
        })
    } else {
        None
    };
    let mut application = Application {
        input_sender,
        fast_path_enabled,
        ..Application::default()
    };

    event_loop
        .run_app(&mut application)
        .context("run event loop")?;

    if let Some(error) = application.error {
        return Err(error);
    }

    Ok(())
}

#[derive(Debug)]
enum ClientEvent {
    DmabufFrame {
        resource_id: u64,
        resource: Option<DmabufFrame>,
    },
}

fn spawn_frame_updates(
    target: ControlTarget,
    mut frames: manifold_ipc::CriticalConsumer,
    proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
) -> Result<()> {
    let _worker = thread::Builder::new()
        .name("manifold-frame-updates".to_string())
        .spawn(move || {
            let mut resources = HashSet::new();
            loop {
                let record = frames.recv_spinning();
                if record.opcode != manifold_ipc::FRAME_DMABUF_COMMIT {
                    continue;
                }
                let resource_id = record.arguments[0];
                drop(record);
                let resource = if resources.insert(resource_id) {
                    match dmabuf_resource(&target, resource_id) {
                        Ok(resource) => Some(resource),
                        Err(_) => continue,
                    }
                } else {
                    None
                };
                if proxy
                    .send_event(ClientEvent::DmabufFrame {
                        resource_id,
                        resource,
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .context("spawn DMA-BUF frame update worker")?;
    Ok(())
}

struct InputLane {
    producer: manifold_ipc::CriticalProducer,
    generation: u64,
    sequence: u64,
}

#[derive(Default)]
struct Application {
    renderer: Option<Renderer>,
    error: Option<anyhow::Error>,
    input_sender: Option<InputLane>,
    fast_path_enabled: bool,
}

impl ApplicationHandler<ClientEvent> for Application {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }

        let result = event_loop
            .create_window(Window::default_attributes().with_title("Manifold"))
            .context("create presentation window")
            .and_then(|window| Renderer::new(Arc::new(window), self.fast_path_enabled));

        match result {
            Ok(renderer) => {
                renderer.window.request_redraw();
                self.renderer = Some(renderer);
            }
            Err(error) => {
                self.error = Some(error);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: ClientEvent) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        match event {
            ClientEvent::DmabufFrame {
                resource_id,
                resource,
            } => {
                if let Err(error) = renderer.select_frame(resource_id, resource) {
                    self.error = Some(error);
                    event_loop.exit();
                }
            }
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
                renderer.resize(size.width, size.height);
                if let Some(sender) = self.input_sender.as_mut() {
                    send_input(
                        sender,
                        manifold_ipc::INPUT_PRESENTATION_SIZE,
                        [
                            u64::from(size.width.max(1)),
                            u64::from(size.height.max(1)),
                            0,
                            0,
                            0,
                        ],
                    );
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let (Some(sender), Some((content_width, content_height))) =
                    (self.input_sender.as_mut(), renderer.content_extent())
                {
                    let window_size = renderer.window.inner_size();
                    let x =
                        position.x * f64::from(content_width) / f64::from(window_size.width.max(1));
                    let y = position.y * f64::from(content_height)
                        / f64::from(window_size.height.max(1));
                    send_input(
                        sender,
                        manifold_ipc::INPUT_POINTER_MOTION,
                        [x.to_bits(), y.to_bits(), 0, 0, 0],
                    );
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
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
                if let Some(sender) = self.input_sender.as_mut() {
                    let (horizontal, vertical) = match delta {
                        MouseScrollDelta::LineDelta(x, y) => {
                            (f64::from(x) * 15.0, f64::from(y) * 15.0)
                        }
                        MouseScrollDelta::PixelDelta(position) => (position.x, position.y),
                    };
                    send_input(
                        sender,
                        manifold_ipc::INPUT_POINTER_AXIS,
                        [horizontal.to_bits(), vertical.to_bits(), 0, 0, 0],
                    );
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } if !is_synthetic && !event.repeat => {
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
                RenderOutcome::Presented | RenderOutcome::Skipped => {}
                RenderOutcome::Reconfigure => renderer.reconfigure(),
                RenderOutcome::Failed(message) => {
                    self.error = Some(anyhow!(message));
                    event_loop.exit();
                }
            },
            _ => {}
        }
    }
}

struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    imported_frames: HashMap<u64, ImportedFrame>,
    current_frame: Option<u64>,
}

struct ImportedFrame {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    width: u32,
    height: u32,
}

fn send_input(lane: &mut InputLane, opcode: u32, arguments: [u64; 5]) {
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
    }
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

impl ImportedFrame {
    fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        frame: DmabufFrame,
    ) -> Result<Self> {
        const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
        const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
        ensure_supported_fourcc(frame.fourcc, DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888)?;
        let texture = import_single_plane(
            device,
            frame.fd,
            SinglePlaneDmabuf {
                width: frame.width,
                height: frame.height,
                format: wgpu::TextureFormat::Bgra8Unorm,
                modifier: frame.modifier,
                stride: u64::from(frame.stride),
                offset: u64::from(frame.offset),
            },
        )?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
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
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("hosted DMA-BUF bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
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
                        vec2<f32>(0.0, 1.0), vec2<f32>(2.0, 1.0), vec2<f32>(0.0, -1.0));
                    var output: VertexOutput;
                    output.position = vec4<f32>(positions[index], 0.0, 1.0);
                    output.uv = uvs[index];
                    return output;
                }
                @group(0) @binding(0) var image: texture_2d<f32>;
                @group(0) @binding(1) var image_sampler: sampler;
                @fragment fn fragment(input: VertexOutput) -> @location(0) vec4<f32> {
                    return textureSample(image, image_sampler, input.uv);
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
                entry_point: Some("fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
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
        eprintln!(
            "imported hosted DMA-BUF sequence={} size={}x{} stride={} modifier={} y_inverted={}",
            frame.sequence,
            frame.width,
            frame.height,
            frame.stride,
            frame.modifier,
            frame.y_inverted
        );
        Ok(Self {
            _texture: texture,
            bind_group,
            pipeline,
            width: frame.width,
            height: frame.height,
        })
    }
}

fn ensure_supported_fourcc(fourcc: u32, argb: u32, xrgb: u32) -> Result<()> {
    if fourcc == argb || fourcc == xrgb {
        Ok(())
    } else {
        Err(anyhow!("unsupported happy-path DMA-BUF fourcc {fourcc:#x}"))
    }
}

impl Renderer {
    fn new(window: Arc<Window>, fast_path_enabled: bool) -> Result<Self> {
        pollster::block_on(Self::new_async(window, fast_path_enabled))
    }

    async fn new_async(window: Arc<Window>, fast_path_enabled: bool) -> Result<Self> {
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
                power_preference: wgpu::PowerPreference::default(),
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

        let size = window.inner_size();
        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| anyhow!("wgpu surface has no supported configuration"))?;
        surface.configure(&device, &config);
        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            imported_frames: HashMap::new(),
            current_frame: None,
        })
    }

    fn content_extent(&self) -> Option<(u32, u32)> {
        self.current_frame
            .and_then(|id| self.imported_frames.get(&id))
            .map(|frame| (frame.width, frame.height))
    }

    fn select_frame(&mut self, resource_id: u64, resource: Option<DmabufFrame>) -> Result<()> {
        if let Some(frame) = resource {
            let imported = ImportedFrame::new(&self.device, self.config.format, frame)?;
            self.imported_frames.insert(resource_id, imported);
        }
        if self.imported_frames.contains_key(&resource_id) {
            self.current_frame = Some(resource_id);
            self.window.request_redraw();
        }
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }

        self.config.width = width;
        self.config.height = height;
        self.reconfigure();
        self.window.request_redraw();
    }

    fn reconfigure(&self) {
        self.surface.configure(&self.device, &self.config);
        self.window.request_redraw();
    }

    fn render(&self) -> RenderOutcome {
        let (frame, outcome) = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(frame) => (frame, RenderOutcome::Presented),
            CurrentSurfaceTexture::Suboptimal(frame) => (frame, RenderOutcome::Reconfigure),
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
            if let Some(imported) = self
                .current_frame
                .and_then(|id| self.imported_frames.get(&id))
            {
                render_pass.set_pipeline(&imported.pipeline);
                render_pass.set_bind_group(0, &imported.bind_group, &[]);
                render_pass.draw(0..3, 0..1);
            }
            drop(render_pass);
        }

        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        outcome
    }
}

#[derive(Clone, Copy)]
enum RenderOutcome {
    Presented,
    Reconfigure,
    Skipped,
    Failed(&'static str),
}
