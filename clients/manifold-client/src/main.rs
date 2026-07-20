use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use manifold_cli::{
    ClientArgs,
    control::{ServerLaunch, connect_or_spawn, resolve_control_target},
};
use wgpu::{Color, CurrentSurfaceTexture};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
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

    let event_loop = EventLoop::new().context("create event loop")?;
    let mut application = Application::default();

    event_loop
        .run_app(&mut application)
        .context("run event loop")?;

    if let Some(error) = application.error {
        return Err(error);
    }

    Ok(())
}

#[derive(Default)]
struct Application {
    renderer: Option<Renderer>,
    error: Option<anyhow::Error>,
}

impl ApplicationHandler for Application {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }

        let result = event_loop
            .create_window(Window::default_attributes().with_title("Manifold"))
            .context("create presentation window")
            .and_then(|window| Renderer::new(Arc::new(window)));

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
            WindowEvent::Resized(size) => renderer.resize(size.width, size.height),
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
}

impl Renderer {
    fn new(window: Arc<Window>) -> Result<Self> {
        pollster::block_on(Self::new_async(window))
    }

    async fn new_async(window: Arc<Window>) -> Result<Self> {
        let instance = wgpu::Instance::default();
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
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("manifold-client device"),
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
        })
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
            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
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
