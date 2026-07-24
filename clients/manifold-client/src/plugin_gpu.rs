use std::{
    collections::BTreeMap,
    mem,
    ops::Range,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use bytemuck::{Pod, Zeroable};
use lyon_tessellation::{
    FillGeometryBuilder, FillOptions, FillRule, FillTessellator, FillVertex, GeometryBuilder,
    GeometryBuilderError, LineCap, LineJoin, StrokeGeometryBuilder, StrokeOptions,
    StrokeTessellator, StrokeVertex, TessellationError, VertexBuffers, VertexId,
    math::{Point, point},
    path::Path as LyonPath,
};
use manifold_graphics::{BlendMode, Color, Command, Path, PathSegment, Rect, Transform};
use manifold_wasm::PresentationError;
use thiserror::Error;
use wgpu::util::DeviceExt;

use crate::plugin_graphics::{GraphicsSnapshot, ImageResource, TextLayoutResource};

const CANVAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const RESOURCE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const STENCIL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24PlusStencil8;
const SAMPLE_COUNT: u32 = 4;
const SLOW_FRAME: Duration = Duration::from_millis(8);
const MAX_SCENE_VERTICES: usize = 262_144;
const MAX_SCENE_INDICES: usize = 786_432;
const TARGET_CURVE_ERROR_PIXELS: f32 = 0.25;
const MIN_TESSELLATION_TOLERANCE: f32 = 0.01;
const MAX_TESSELLATION_SCALE: f32 = 8.0;

#[derive(Debug, Error)]
enum SceneCompileError {
    #[error("plugin GPU scene complexity limit exceeded")]
    Limit,
    #[error("plugin GPU scene is not renderable: {0}")]
    Invalid(String),
}

impl SceneCompileError {
    const fn presentation_error(&self) -> PresentationError {
        match self {
            Self::Limit => PresentationError::LimitReached,
            Self::Invalid(_) => PresentationError::InvalidPacket,
        }
    }
}

type CompileResult<T> = std::result::Result<T, SceneCompileError>;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CanvasVertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 3] =
    wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4];

impl CanvasVertex {
    const fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &VERTEX_ATTRIBUTES,
        }
    }
}

#[derive(Clone, Debug)]
struct Geometry {
    indices: Range<u32>,
}

#[derive(Clone, Debug)]
enum DrawSource {
    White,
    Resource(u64),
    Layer(usize),
}

#[derive(Clone, Debug)]
struct DrawOp {
    geometry: Geometry,
    clips: Vec<Geometry>,
    source: DrawSource,
    blend_mode: BlendMode,
}

#[derive(Clone, Debug)]
enum CanvasOp {
    Draw(DrawOp),
    BeginLayer(usize),
    EndLayer { layer: usize, draw: DrawOp },
}

#[derive(Clone, Debug)]
struct CompileState {
    transform: Transform,
    opacity: f32,
    blend_mode: BlendMode,
    clips: Vec<Geometry>,
}

impl Default for CompileState {
    fn default() -> Self {
        Self {
            transform: Transform::IDENTITY,
            opacity: 1.0,
            blend_mode: BlendMode::SourceOver,
            clips: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct GpuTexture {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

#[derive(Clone)]
struct CanvasTarget {
    _multisample_texture: wgpu::Texture,
    multisample_view: wgpu::TextureView,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

#[derive(Clone)]
struct StencilTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}

pub struct GpuCanvasScene {
    pub pane: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    ops: Vec<CanvasOp>,
    resources: BTreeMap<u64, GpuTexture>,
    root: CanvasTarget,
    layers: Vec<CanvasTarget>,
    scratch: CanvasTarget,
    stencil: StencilTarget,
    dirty: bool,
}

impl GpuCanvasScene {
    pub const fn output_bind_group(&self) -> &wgpu::BindGroup {
        &self.root.bind_group
    }
}

pub struct GpuCanvasRenderer {
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    white: GpuTexture,
    clip_pipeline: wgpu::RenderPipeline,
    source_over_pipeline: wgpu::RenderPipeline,
    screen_pipeline: wgpu::RenderPipeline,
    plus_pipeline: wgpu::RenderPipeline,
    multiply_pipeline: wgpu::RenderPipeline,
    overlay_pipeline: wgpu::RenderPipeline,
    darken_pipeline: wgpu::RenderPipeline,
    lighten_pipeline: wgpu::RenderPipeline,
    difference_pipeline: wgpu::RenderPipeline,
}

pub fn validate_snapshot_lowering(
    snapshot: &GraphicsSnapshot,
) -> std::result::Result<(), PresentationError> {
    let mut compiler = SceneCompiler::new(1, 1, MAX_TESSELLATION_SCALE);
    compiler
        .compile(snapshot)
        .map_err(|error| error.presentation_error())
}

impl GpuCanvasRenderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture_layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("plugin GPU canvas shader"),
            source: wgpu::ShaderSource::Wgsl(CANVAS_SHADER.into()),
        });
        let geometry_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("plugin GPU canvas geometry layout"),
            bind_group_layouts: &[Some(texture_layout)],
            immediate_size: 0,
        });
        let advanced_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("plugin GPU canvas advanced blend layout"),
            bind_group_layouts: &[Some(texture_layout), Some(texture_layout)],
            immediate_size: 0,
        });
        let clip_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("plugin GPU canvas clip layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let clip_pipeline = create_pipeline(
            device,
            &shader,
            &clip_layout,
            Some("fragment_clip"),
            None,
            StencilKind::Increment,
            "plugin GPU canvas clip pipeline",
        );
        let source_over_pipeline = create_pipeline(
            device,
            &shader,
            &geometry_layout,
            Some("fragment_source"),
            Some(source_over_blend()),
            StencilKind::Test,
            "plugin GPU canvas source-over pipeline",
        );
        let screen_pipeline = create_pipeline(
            device,
            &shader,
            &geometry_layout,
            Some("fragment_source"),
            Some(screen_blend()),
            StencilKind::Test,
            "plugin GPU canvas screen pipeline",
        );
        let plus_pipeline = create_pipeline(
            device,
            &shader,
            &geometry_layout,
            Some("fragment_source"),
            Some(plus_blend()),
            StencilKind::Test,
            "plugin GPU canvas plus pipeline",
        );
        let multiply_pipeline = create_pipeline(
            device,
            &shader,
            &advanced_layout,
            Some("fragment_multiply"),
            None,
            StencilKind::Test,
            "plugin GPU canvas multiply pipeline",
        );
        let overlay_pipeline = create_pipeline(
            device,
            &shader,
            &advanced_layout,
            Some("fragment_overlay"),
            None,
            StencilKind::Test,
            "plugin GPU canvas overlay pipeline",
        );
        let darken_pipeline = create_pipeline(
            device,
            &shader,
            &advanced_layout,
            Some("fragment_darken"),
            None,
            StencilKind::Test,
            "plugin GPU canvas darken pipeline",
        );
        let lighten_pipeline = create_pipeline(
            device,
            &shader,
            &advanced_layout,
            Some("fragment_lighten"),
            None,
            StencilKind::Test,
            "plugin GPU canvas lighten pipeline",
        );
        let difference_pipeline = create_pipeline(
            device,
            &shader,
            &advanced_layout,
            Some("fragment_difference"),
            None,
            StencilKind::Test,
            "plugin GPU canvas difference pipeline",
        );
        let white = upload_texture(
            device,
            queue,
            texture_layout,
            sampler,
            (1, 1),
            &[u8::MAX; 4],
            "plugin GPU canvas white texture",
        );
        Self {
            texture_layout: texture_layout.clone(),
            sampler: sampler.clone(),
            white,
            clip_pipeline,
            source_over_pipeline,
            screen_pipeline,
            plus_pipeline,
            multiply_pipeline,
            overlay_pipeline,
            darken_pipeline,
            lighten_pipeline,
            difference_pipeline,
        }
    }

    pub fn compile(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        snapshot: &GraphicsSnapshot,
        extent: (u32, u32, f32),
        previous: Option<&GpuCanvasScene>,
    ) -> Result<GpuCanvasScene> {
        let started = Instant::now();
        let (width, height, scale) = extent;
        let mut compiler = SceneCompiler::new(width, height, scale);
        compiler.compile(snapshot)?;
        let SceneCompiler {
            vertices,
            indices,
            ops,
            layer_count,
            ..
        } = compiler;
        let fallback_vertex = CanvasVertex::zeroed();
        let fallback_index = 0_u32;
        let vertex_bytes = if vertices.is_empty() {
            bytemuck::bytes_of(&fallback_vertex)
        } else {
            bytemuck::cast_slice(&vertices)
        };
        let index_bytes = if indices.is_empty() {
            bytemuck::bytes_of(&fallback_index)
        } else {
            bytemuck::cast_slice(&indices)
        };
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("plugin GPU canvas vertices"),
            contents: vertex_bytes,
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("plugin GPU canvas indices"),
            contents: index_bytes,
            usage: wgpu::BufferUsages::INDEX,
        });
        let mut resources = BTreeMap::new();
        for (handle, resource) in &snapshot.images {
            resources.insert(
                *handle,
                previous
                    .and_then(|scene| scene.resources.get(handle))
                    .cloned()
                    .unwrap_or_else(|| self.upload_image(device, queue, *handle, resource)),
            );
        }
        for (handle, resource) in &snapshot.text_layouts {
            resources.insert(
                *handle,
                previous
                    .and_then(|scene| scene.resources.get(handle))
                    .cloned()
                    .unwrap_or_else(|| self.upload_text(device, queue, *handle, resource)),
            );
        }
        let reusable = previous.filter(|scene| scene.width == width && scene.height == height);
        let root = reusable.map_or_else(
            || self.create_target(device, width, height, "plugin GPU canvas root"),
            |scene| scene.root.clone(),
        );
        let scratch = reusable.map_or_else(
            || self.create_target(device, width, height, "plugin GPU canvas blend scratch"),
            |scene| scene.scratch.clone(),
        );
        let stencil = reusable.map_or_else(
            || create_stencil(device, width, height),
            |scene| scene.stencil.clone(),
        );
        let mut layers = Vec::with_capacity(layer_count);
        for index in 0..layer_count {
            layers.push(
                reusable
                    .and_then(|scene| scene.layers.get(index))
                    .cloned()
                    .unwrap_or_else(|| {
                        self.create_target(
                            device,
                            width,
                            height,
                            &format!("plugin GPU canvas layer {index}"),
                        )
                    }),
            );
        }
        let elapsed = started.elapsed();
        if snapshot.sequence <= 3 || snapshot.sequence.is_multiple_of(120) || elapsed >= SLOW_FRAME
        {
            eprintln!(
                "compiled plugin GPU scene pane={} sequence={} commands={} draws={} vertices={} indices={} layers={} time_us={}",
                snapshot.pane,
                snapshot.sequence,
                snapshot.packet.commands.len(),
                ops.iter()
                    .filter(|op| !matches!(op, CanvasOp::BeginLayer(_)))
                    .count(),
                vertices.len(),
                indices.len(),
                layer_count,
                elapsed.as_micros(),
            );
        }
        Ok(GpuCanvasScene {
            pane: snapshot.pane,
            sequence: snapshot.sequence,
            width,
            height,
            vertex_buffer,
            index_buffer,
            ops,
            resources,
            root,
            layers,
            scratch,
            stencil,
            dirty: true,
        })
    }

    pub fn encode_if_dirty(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        scene: &mut GpuCanvasScene,
    ) -> Result<bool> {
        if !scene.dirty {
            return Ok(false);
        }
        let started = Instant::now();
        clear_target(encoder, &scene.root, "plugin GPU canvas clear root");
        let mut target_stack = vec![TargetId::Root];
        for op in &scene.ops {
            match op {
                CanvasOp::Draw(draw) => {
                    let target = *target_stack
                        .last()
                        .context("plugin GPU canvas lost current target")?;
                    self.encode_draw(encoder, scene, target, draw)?;
                }
                CanvasOp::BeginLayer(layer) => {
                    let target = scene
                        .layers
                        .get(*layer)
                        .context("plugin GPU canvas lost layer target")?;
                    clear_target(encoder, target, "plugin GPU canvas clear layer");
                    target_stack.push(TargetId::Layer(*layer));
                }
                CanvasOp::EndLayer { layer, draw } => {
                    let popped = target_stack
                        .pop()
                        .context("plugin GPU canvas layer stack underflow")?;
                    if popped != TargetId::Layer(*layer) {
                        return Err(anyhow!("plugin GPU canvas layer order changed"));
                    }
                    let target = *target_stack
                        .last()
                        .context("plugin GPU canvas layer lost parent")?;
                    self.encode_draw(encoder, scene, target, draw)?;
                }
            }
        }
        if target_stack != [TargetId::Root] {
            return Err(anyhow!("plugin GPU canvas layer stack remained open"));
        }
        scene.dirty = false;
        let elapsed = started.elapsed();
        if elapsed >= SLOW_FRAME {
            eprintln!(
                "slow plugin GPU command encoding pane={} sequence={} time_us={}",
                scene.pane,
                scene.sequence,
                elapsed.as_micros(),
            );
        }
        Ok(true)
    }

    fn encode_draw(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        scene: &GpuCanvasScene,
        target_id: TargetId,
        draw: &DrawOp,
    ) -> Result<()> {
        let target = target(scene, target_id)?;
        if is_advanced_blend(draw.blend_mode) {
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &target.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &scene.scratch.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: scene.width,
                    height: scene.height,
                    depth_or_array_layers: 1,
                },
            );
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("plugin GPU canvas draw"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target.multisample_view,
                resolve_target: Some(&target.view),
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &scene.stencil.view,
                depth_ops: None,
                stencil_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0),
                    store: wgpu::StoreOp::Discard,
                }),
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_vertex_buffer(0, scene.vertex_buffer.slice(..));
        pass.set_index_buffer(scene.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        if !draw.clips.is_empty() {
            pass.set_pipeline(&self.clip_pipeline);
            for (depth, clip) in draw.clips.iter().enumerate() {
                pass.set_stencil_reference(u32::try_from(depth).unwrap_or(u32::MAX));
                pass.draw_indexed(clip.indices.clone(), 0, 0..1);
            }
        }
        pass.set_pipeline(self.pipeline(draw.blend_mode));
        pass.set_stencil_reference(u32::try_from(draw.clips.len()).unwrap_or(u32::MAX));
        pass.set_bind_group(0, self.source_bind_group(scene, &draw.source)?, &[]);
        if is_advanced_blend(draw.blend_mode) {
            pass.set_bind_group(1, &scene.scratch.bind_group, &[]);
        }
        pass.draw_indexed(draw.geometry.indices.clone(), 0, 0..1);
        drop(pass);
        Ok(())
    }

    fn source_bind_group<'a>(
        &'a self,
        scene: &'a GpuCanvasScene,
        source: &DrawSource,
    ) -> Result<&'a wgpu::BindGroup> {
        match source {
            DrawSource::White => Ok(&self.white.bind_group),
            DrawSource::Resource(handle) => scene
                .resources
                .get(handle)
                .map(|resource| &resource.bind_group)
                .with_context(|| format!("plugin GPU resource {handle} disappeared")),
            DrawSource::Layer(layer) => scene
                .layers
                .get(*layer)
                .map(|target| &target.bind_group)
                .with_context(|| format!("plugin GPU layer {layer} disappeared")),
        }
    }

    const fn pipeline(&self, mode: BlendMode) -> &wgpu::RenderPipeline {
        match mode {
            BlendMode::SourceOver => &self.source_over_pipeline,
            BlendMode::Multiply => &self.multiply_pipeline,
            BlendMode::Screen => &self.screen_pipeline,
            BlendMode::Overlay => &self.overlay_pipeline,
            BlendMode::Darken => &self.darken_pipeline,
            BlendMode::Lighten => &self.lighten_pipeline,
            BlendMode::Difference => &self.difference_pipeline,
            BlendMode::Plus => &self.plus_pipeline,
        }
    }

    fn upload_image(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        handle: u64,
        resource: &ImageResource,
    ) -> GpuTexture {
        upload_texture(
            device,
            queue,
            &self.texture_layout,
            &self.sampler,
            (resource.width, resource.height),
            &resource.linear_premultiplied_rgba8,
            &format!("plugin GPU image {handle}"),
        )
    }

    fn upload_text(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        handle: u64,
        resource: &TextLayoutResource,
    ) -> GpuTexture {
        upload_texture(
            device,
            queue,
            &self.texture_layout,
            &self.sampler,
            (resource.width, resource.height),
            &resource.linear_premultiplied_rgba8,
            &format!("plugin GPU text layout {handle}"),
        )
    }

    fn create_target(
        &self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        label: &str,
    ) -> CanvasTarget {
        let multisample_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(&format!("{label} multisample")),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: SAMPLE_COUNT,
            dimension: wgpu::TextureDimension::D2,
            format: CANVAS_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let multisample_view =
            multisample_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: CANVAS_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group =
            texture_bind_group(device, &self.texture_layout, &self.sampler, &view, label);
        CanvasTarget {
            _multisample_texture: multisample_texture,
            multisample_view,
            texture,
            view,
            bind_group,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetId {
    Root,
    Layer(usize),
}

fn target(scene: &GpuCanvasScene, id: TargetId) -> Result<&CanvasTarget> {
    match id {
        TargetId::Root => Ok(&scene.root),
        TargetId::Layer(layer) => scene
            .layers
            .get(layer)
            .with_context(|| format!("plugin GPU target layer {layer} disappeared")),
    }
}

struct SceneCompiler {
    width: u32,
    height: u32,
    scale: f32,
    vertices: Vec<CanvasVertex>,
    indices: Vec<u32>,
    ops: Vec<CanvasOp>,
    layer_count: usize,
}

impl SceneCompiler {
    const fn new(width: u32, height: u32, scale: f32) -> Self {
        Self {
            width,
            height,
            scale,
            vertices: Vec::new(),
            indices: Vec::new(),
            ops: Vec::new(),
            layer_count: 0,
        }
    }

    fn compile(&mut self, snapshot: &GraphicsSnapshot) -> CompileResult<()> {
        let mut state = CompileState::default();
        let mut saved = Vec::new();
        let mut layers = Vec::new();
        for command in &snapshot.packet.commands {
            match command {
                Command::Save => saved.push(state.clone()),
                Command::Restore => {
                    state = saved.pop().ok_or_else(|| {
                        SceneCompileError::Invalid("restore stack underflow".to_owned())
                    })?;
                }
                Command::SetTransform(transform) => state.transform = *transform,
                Command::SetOpacity(opacity) => state.opacity = *opacity,
                Command::SetBlendMode(mode) => state.blend_mode = *mode,
                Command::ClipRect(rect) => {
                    let geometry =
                        self.fill_geometry(&rect_path(*rect), state.transform, [1.0; 4])?;
                    state.clips.push(geometry);
                }
                Command::ClipPath(path) => {
                    let geometry = self.fill_geometry(path, state.transform, [1.0; 4])?;
                    state.clips.push(geometry);
                }
                Command::FillRect { rect, color } => {
                    let geometry = self.fill_geometry(
                        &rect_path(*rect),
                        state.transform,
                        premultiplied(*color, state.opacity),
                    )?;
                    self.push_draw(geometry, DrawSource::White, &state);
                }
                Command::StrokeRect { rect, color, width } => {
                    let geometry = self.stroke_geometry(
                        &rect_path(*rect),
                        state.transform,
                        premultiplied(*color, state.opacity),
                        *width,
                    )?;
                    self.push_draw(geometry, DrawSource::White, &state);
                }
                Command::FillRoundedRect {
                    rect,
                    radius,
                    color,
                } => {
                    let geometry = self.fill_geometry(
                        &rounded_rect_path(*rect, *radius),
                        state.transform,
                        premultiplied(*color, state.opacity),
                    )?;
                    self.push_draw(geometry, DrawSource::White, &state);
                }
                Command::StrokeRoundedRect {
                    rect,
                    radius,
                    color,
                    width,
                } => {
                    let geometry = self.stroke_geometry(
                        &rounded_rect_path(*rect, *radius),
                        state.transform,
                        premultiplied(*color, state.opacity),
                        *width,
                    )?;
                    self.push_draw(geometry, DrawSource::White, &state);
                }
                Command::FillPath { path, color } => {
                    let geometry = self.fill_geometry(
                        path,
                        state.transform,
                        premultiplied(*color, state.opacity),
                    )?;
                    self.push_draw(geometry, DrawSource::White, &state);
                }
                Command::StrokePath { path, color, width } => {
                    let geometry = self.stroke_geometry(
                        path,
                        state.transform,
                        premultiplied(*color, state.opacity),
                        *width,
                    )?;
                    self.push_draw(geometry, DrawSource::White, &state);
                }
                Command::DrawImage { image, destination } => {
                    if destination.width == 0.0 || destination.height == 0.0 {
                        continue;
                    }
                    let geometry = self.quad_geometry(
                        *destination,
                        state.transform,
                        [state.opacity; 4],
                        false,
                    )?;
                    self.push_draw(geometry, DrawSource::Resource(*image), &state);
                }
                Command::DrawTextLayout { layout, x, y } => {
                    let resource = snapshot.text_layouts.get(layout).ok_or_else(|| {
                        SceneCompileError::Invalid(format!("text layout {layout} is unavailable"))
                    })?;
                    let geometry = self.quad_geometry(
                        Rect::new(*x, *y, resource.width as f32, resource.height as f32),
                        state.transform,
                        [state.opacity; 4],
                        false,
                    )?;
                    self.push_draw(geometry, DrawSource::Resource(*layout), &state);
                }
                Command::BeginLayer => {
                    let layer = layers.len();
                    self.layer_count = self.layer_count.max(layer.saturating_add(1));
                    layers.push(layer);
                    self.ops.push(CanvasOp::BeginLayer(layer));
                }
                Command::EndLayer => {
                    let layer = layers.pop().ok_or_else(|| {
                        SceneCompileError::Invalid("layer stack underflow".to_owned())
                    })?;
                    let logical_width = self.width as f32 / self.scale;
                    let logical_height = self.height as f32 / self.scale;
                    let geometry = self.quad_geometry(
                        Rect::new(0.0, 0.0, logical_width, logical_height),
                        Transform::IDENTITY,
                        [1.0; 4],
                        true,
                    )?;
                    self.ops.push(CanvasOp::EndLayer {
                        layer,
                        draw: DrawOp {
                            geometry,
                            clips: state.clips.clone(),
                            source: DrawSource::Layer(layer),
                            blend_mode: BlendMode::SourceOver,
                        },
                    });
                }
            }
        }
        Ok(())
    }

    fn push_draw(&mut self, geometry: Geometry, source: DrawSource, state: &CompileState) {
        self.ops.push(CanvasOp::Draw(DrawOp {
            geometry,
            clips: state.clips.clone(),
            source,
            blend_mode: state.blend_mode,
        }));
    }

    fn fill_geometry(
        &mut self,
        path: &Path,
        transform: Transform,
        color: [f32; 4],
    ) -> CompileResult<Geometry> {
        let path = lyon_path(path);
        let mut geometry = VertexBuffers::<Point, u32>::new();
        let mut builder = LimitedGeometryBuilder::new(
            &mut geometry,
            MAX_SCENE_VERTICES.saturating_sub(self.vertices.len()),
            MAX_SCENE_INDICES.saturating_sub(self.indices.len()),
        );
        let result = FillTessellator::new().tessellate_path(
            &path,
            &FillOptions::tolerance(self.tessellation_tolerance(transform))
                .with_fill_rule(FillRule::NonZero),
            &mut builder,
        );
        check_tessellation(result, builder.overflowed())?;
        self.append_tessellation(geometry, transform, color)
    }

    fn stroke_geometry(
        &mut self,
        path: &Path,
        transform: Transform,
        color: [f32; 4],
        width: f32,
    ) -> CompileResult<Geometry> {
        let path = lyon_path(path);
        let mut geometry = VertexBuffers::<Point, u32>::new();
        let mut builder = LimitedGeometryBuilder::new(
            &mut geometry,
            MAX_SCENE_VERTICES.saturating_sub(self.vertices.len()),
            MAX_SCENE_INDICES.saturating_sub(self.indices.len()),
        );
        let result = StrokeTessellator::new().tessellate_path(
            &path,
            &StrokeOptions::tolerance(self.tessellation_tolerance(transform))
                .with_line_width(width)
                .with_line_cap(LineCap::Round)
                .with_line_join(LineJoin::Round),
            &mut builder,
        );
        check_tessellation(result, builder.overflowed())?;
        self.append_tessellation(geometry, transform, color)
    }

    fn append_tessellation(
        &mut self,
        geometry: VertexBuffers<Point, u32>,
        transform: Transform,
        color: [f32; 4],
    ) -> CompileResult<Geometry> {
        self.ensure_capacity(geometry.vertices.len(), geometry.indices.len())?;
        let base_vertex =
            u32::try_from(self.vertices.len()).map_err(|_overflow| SceneCompileError::Limit)?;
        let first_index =
            u32::try_from(self.indices.len()).map_err(|_overflow| SceneCompileError::Limit)?;
        let width = self.width;
        let height = self.height;
        let scale = self.scale;
        self.vertices
            .extend(geometry.vertices.into_iter().map(|position| CanvasVertex {
                position: ndc(
                    width,
                    height,
                    scale,
                    transform_point(transform, position.x, position.y),
                ),
                uv: [0.5, 0.5],
                color,
            }));
        for index in geometry.indices {
            self.indices.push(
                base_vertex
                    .checked_add(index)
                    .ok_or(SceneCompileError::Limit)?,
            );
        }
        let last_index =
            u32::try_from(self.indices.len()).map_err(|_overflow| SceneCompileError::Limit)?;
        Ok(Geometry {
            indices: first_index..last_index,
        })
    }

    fn quad_geometry(
        &mut self,
        rect: Rect,
        transform: Transform,
        color: [f32; 4],
        flip_vertical: bool,
    ) -> CompileResult<Geometry> {
        self.ensure_capacity(4, 6)?;
        let base_vertex =
            u32::try_from(self.vertices.len()).map_err(|_overflow| SceneCompileError::Limit)?;
        let first_index =
            u32::try_from(self.indices.len()).map_err(|_overflow| SceneCompileError::Limit)?;
        let right = rect.x + rect.width;
        let bottom = rect.y + rect.height;
        let (top_uv, bottom_uv) = if flip_vertical {
            (1.0, 0.0)
        } else {
            (0.0, 1.0)
        };
        for ((x, y), uv) in [
            ((rect.x, rect.y), [0.0, top_uv]),
            ((right, rect.y), [1.0, top_uv]),
            ((right, bottom), [1.0, bottom_uv]),
            ((rect.x, bottom), [0.0, bottom_uv]),
        ] {
            self.vertices.push(CanvasVertex {
                position: self.ndc(transform_point(transform, x, y)),
                uv,
                color,
            });
        }
        self.indices.extend_from_slice(&[
            base_vertex,
            base_vertex + 1,
            base_vertex + 2,
            base_vertex,
            base_vertex + 2,
            base_vertex + 3,
        ]);
        Ok(Geometry {
            indices: first_index..first_index + 6,
        })
    }

    fn ensure_capacity(&self, vertices: usize, indices: usize) -> CompileResult<()> {
        if self
            .vertices
            .len()
            .checked_add(vertices)
            .is_none_or(|count| count > MAX_SCENE_VERTICES)
            || self
                .indices
                .len()
                .checked_add(indices)
                .is_none_or(|count| count > MAX_SCENE_INDICES)
        {
            return Err(SceneCompileError::Limit);
        }
        Ok(())
    }

    fn tessellation_tolerance(&self, transform: Transform) -> f32 {
        let sx = f64::from(transform.sx);
        let ky = f64::from(transform.ky);
        let kx = f64::from(transform.kx);
        let sy = f64::from(transform.sy);
        let sum_of_squares = sy.mul_add(sy, kx.mul_add(kx, ky.mul_add(ky, sx * sx)));
        let determinant = kx.mul_add(-ky, sx * sy);
        let discriminant = (4.0 * determinant)
            .mul_add(-determinant, sum_of_squares * sum_of_squares)
            .max(0.0);
        let transform_scale = ((sum_of_squares + discriminant.sqrt()) * 0.5)
            .sqrt()
            .max(1.0);
        let device_scale = f64::from(self.scale.clamp(1.0, MAX_TESSELLATION_SCALE));
        (f64::from(TARGET_CURVE_ERROR_PIXELS) / (transform_scale * device_scale)).clamp(
            f64::from(MIN_TESSELLATION_TOLERANCE),
            f64::from(TARGET_CURVE_ERROR_PIXELS),
        ) as f32
    }

    fn ndc(&self, logical: [f32; 2]) -> [f32; 2] {
        let physical_x = logical[0] * self.scale;
        let physical_y = logical[1] * self.scale;
        [
            physical_x * 2.0 / self.width as f32 - 1.0,
            physical_y * 2.0 / self.height as f32 - 1.0,
        ]
    }
}

struct LimitedGeometryBuilder<'a> {
    buffers: &'a mut VertexBuffers<Point, u32>,
    max_vertices: usize,
    max_indices: usize,
    first_vertex: usize,
    first_index: usize,
    overflowed: bool,
}

impl<'a> LimitedGeometryBuilder<'a> {
    const fn new(
        buffers: &'a mut VertexBuffers<Point, u32>,
        max_vertices: usize,
        max_indices: usize,
    ) -> Self {
        Self {
            buffers,
            max_vertices,
            max_indices,
            first_vertex: 0,
            first_index: 0,
            overflowed: false,
        }
    }

    const fn overflowed(&self) -> bool {
        self.overflowed
    }

    fn add_position(
        &mut self,
        position: Point,
    ) -> std::result::Result<VertexId, GeometryBuilderError> {
        if !position.x.is_finite() || !position.y.is_finite() {
            return Err(GeometryBuilderError::InvalidVertex);
        }
        if self.buffers.vertices.len() >= self.max_vertices {
            self.overflowed = true;
            return Err(GeometryBuilderError::TooManyVertices);
        }
        let index = u32::try_from(self.buffers.vertices.len())
            .map_err(|_overflow| GeometryBuilderError::TooManyVertices)?;
        self.buffers.vertices.push(position);
        Ok(VertexId(index))
    }
}

impl GeometryBuilder for LimitedGeometryBuilder<'_> {
    fn begin_geometry(&mut self) {
        self.first_vertex = self.buffers.vertices.len();
        self.first_index = self.buffers.indices.len();
    }

    fn add_triangle(&mut self, a: VertexId, b: VertexId, c: VertexId) {
        if self
            .buffers
            .indices
            .len()
            .checked_add(3)
            .is_none_or(|count| count > self.max_indices)
        {
            self.overflowed = true;
            return;
        }
        self.buffers.indices.extend_from_slice(&[a.0, b.0, c.0]);
    }

    fn abort_geometry(&mut self) {
        self.buffers.vertices.truncate(self.first_vertex);
        self.buffers.indices.truncate(self.first_index);
    }
}

impl FillGeometryBuilder for LimitedGeometryBuilder<'_> {
    fn add_fill_vertex(
        &mut self,
        vertex: FillVertex<'_>,
    ) -> std::result::Result<VertexId, GeometryBuilderError> {
        self.add_position(vertex.position())
    }
}

impl StrokeGeometryBuilder for LimitedGeometryBuilder<'_> {
    fn add_stroke_vertex(
        &mut self,
        vertex: StrokeVertex<'_, '_>,
    ) -> std::result::Result<VertexId, GeometryBuilderError> {
        self.add_position(vertex.position())
    }
}

fn check_tessellation<T>(
    result: std::result::Result<T, TessellationError>,
    overflowed: bool,
) -> CompileResult<()> {
    if overflowed
        || matches!(
            result,
            Err(TessellationError::GeometryBuilder(
                GeometryBuilderError::TooManyVertices
            ))
        )
    {
        return Err(SceneCompileError::Limit);
    }
    result
        .map(|_output| ())
        .map_err(|error| SceneCompileError::Invalid(error.to_string()))
}

fn lyon_path(path: &Path) -> LyonPath {
    let mut builder = LyonPath::builder();
    let mut open = false;
    for segment in &path.segments {
        match *segment {
            PathSegment::MoveTo(x, y) => {
                if open {
                    builder.end(false);
                }
                builder.begin(point(x, y));
                open = true;
            }
            PathSegment::LineTo(x, y) => {
                if open {
                    builder.line_to(point(x, y));
                } else {
                    builder.begin(point(x, y));
                    open = true;
                }
            }
            PathSegment::QuadTo(x1, y1, x, y) => {
                if open {
                    builder.quadratic_bezier_to(point(x1, y1), point(x, y));
                } else {
                    builder.begin(point(x, y));
                    open = true;
                }
            }
            PathSegment::CubicTo(x1, y1, x2, y2, x, y) => {
                if open {
                    builder.cubic_bezier_to(point(x1, y1), point(x2, y2), point(x, y));
                } else {
                    builder.begin(point(x, y));
                    open = true;
                }
            }
            PathSegment::Close if open => {
                builder.close();
                open = false;
            }
            PathSegment::Close => {}
        }
    }
    if open {
        builder.end(false);
    }
    builder.build()
}

fn rect_path(rect: Rect) -> Path {
    Path::new()
        .move_to(rect.x, rect.y)
        .line_to(rect.x + rect.width, rect.y)
        .line_to(rect.x + rect.width, rect.y + rect.height)
        .line_to(rect.x, rect.y + rect.height)
        .close()
}

fn rounded_rect_path(rect: Rect, radius: f32) -> Path {
    let radius = radius.min(rect.width * 0.5).min(rect.height * 0.5);
    if radius <= f32::EPSILON {
        return rect_path(rect);
    }
    let right = rect.x + rect.width;
    let bottom = rect.y + rect.height;
    let control = radius * 0.552_284_8;
    Path::new()
        .move_to(rect.x + radius, rect.y)
        .line_to(right - radius, rect.y)
        .cubic_to(
            (right - radius + control, rect.y),
            (right, rect.y + radius - control),
            (right, rect.y + radius),
        )
        .line_to(right, bottom - radius)
        .cubic_to(
            (right, bottom - radius + control),
            (right - radius + control, bottom),
            (right - radius, bottom),
        )
        .line_to(rect.x + radius, bottom)
        .cubic_to(
            (rect.x + radius - control, bottom),
            (rect.x, bottom - radius + control),
            (rect.x, bottom - radius),
        )
        .line_to(rect.x, rect.y + radius)
        .cubic_to(
            (rect.x, rect.y + radius - control),
            (rect.x + radius - control, rect.y),
            (rect.x + radius, rect.y),
        )
        .close()
}

const fn transform_point(transform: Transform, x: f32, y: f32) -> [f32; 2] {
    [
        transform
            .sx
            .mul_add(x, transform.kx.mul_add(y, transform.tx)),
        transform
            .ky
            .mul_add(x, transform.sy.mul_add(y, transform.ty)),
    ]
}

fn ndc(width: u32, height: u32, scale: f32, logical: [f32; 2]) -> [f32; 2] {
    [
        logical[0] * scale * 2.0 / width as f32 - 1.0,
        logical[1] * scale * 2.0 / height as f32 - 1.0,
    ]
}

fn premultiplied(color: Color, opacity: f32) -> [f32; 4] {
    let alpha = color.a * opacity;
    [
        srgb_to_linear(color.r) * alpha,
        srgb_to_linear(color.g) * alpha,
        srgb_to_linear(color.b) * alpha,
        alpha,
    ]
}

fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

const fn is_advanced_blend(mode: BlendMode) -> bool {
    matches!(
        mode,
        BlendMode::Multiply
            | BlendMode::Overlay
            | BlendMode::Darken
            | BlendMode::Lighten
            | BlendMode::Difference
    )
}

fn upload_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    extent: (u32, u32),
    pixels: &[u8],
    label: &str,
) -> GpuTexture {
    let (width, height) = extent;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: RESOURCE_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width.saturating_mul(4)),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = texture_bind_group(device, layout, sampler, &view, label);
    GpuTexture {
        _texture: texture,
        bind_group,
    }
}

fn texture_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    view: &wgpu::TextureView,
    label: &str,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

fn create_stencil(device: &wgpu::Device, width: u32, height: u32) -> StencilTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("plugin GPU canvas stencil"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: SAMPLE_COUNT,
        dimension: wgpu::TextureDimension::D2,
        format: STENCIL_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    StencilTarget {
        _texture: texture,
        view,
    }
}

fn clear_target(encoder: &mut wgpu::CommandEncoder, target: &CanvasTarget, label: &'static str) {
    let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &target.multisample_view,
            resolve_target: Some(&target.view),
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    drop(pass);
}

#[derive(Clone, Copy)]
enum StencilKind {
    Increment,
    Test,
}

fn create_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    fragment_entry: Option<&str>,
    blend: Option<wgpu::BlendState>,
    stencil_kind: StencilKind,
    label: &str,
) -> wgpu::RenderPipeline {
    let stencil_face = match stencil_kind {
        StencilKind::Increment => wgpu::StencilFaceState {
            compare: wgpu::CompareFunction::Equal,
            fail_op: wgpu::StencilOperation::Keep,
            depth_fail_op: wgpu::StencilOperation::Keep,
            pass_op: wgpu::StencilOperation::IncrementClamp,
        },
        StencilKind::Test => wgpu::StencilFaceState {
            compare: wgpu::CompareFunction::Equal,
            fail_op: wgpu::StencilOperation::Keep,
            depth_fail_op: wgpu::StencilOperation::Keep,
            pass_op: wgpu::StencilOperation::Keep,
        },
    };
    let color_targets = [Some(wgpu::ColorTargetState {
        format: CANVAS_FORMAT,
        blend,
        write_mask: if matches!(stencil_kind, StencilKind::Increment) {
            wgpu::ColorWrites::empty()
        } else {
            wgpu::ColorWrites::ALL
        },
    })];
    let fragment = fragment_entry.map(|entry_point| wgpu::FragmentState {
        module: shader,
        entry_point: Some(entry_point),
        targets: &color_targets,
        compilation_options: wgpu::PipelineCompilationOptions::default(),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vertex"),
            buffers: &[Some(CanvasVertex::layout())],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment,
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: STENCIL_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::Always),
            stencil: wgpu::StencilState {
                front: stencil_face,
                back: stencil_face,
                read_mask: u32::MAX,
                write_mask: if matches!(stencil_kind, StencilKind::Increment) {
                    u32::MAX
                } else {
                    0
                },
            },
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState {
            count: SAMPLE_COUNT,
            ..Default::default()
        },
        multiview_mask: None,
        cache: None,
    })
}

const fn source_over_blend() -> wgpu::BlendState {
    wgpu::BlendState {
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
    }
}

const fn screen_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrc,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

const fn plus_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

const CANVAS_SHADER: &str = r"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex fn vertex(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = vec4<f32>(input.position, 0.0, 1.0);
    output.uv = input.uv;
    output.color = input.color;
    return output;
}

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
@group(1) @binding(0) var backdrop_texture: texture_2d<f32>;
@group(1) @binding(1) var backdrop_sampler: sampler;

fn source(input: VertexOutput) -> vec4<f32> {
    return textureSample(source_texture, source_sampler, input.uv) * input.color;
}

@fragment fn fragment_clip() -> @location(0) vec4<f32> {
    return vec4<f32>(0.0);
}

@fragment fn fragment_source(input: VertexOutput) -> @location(0) vec4<f32> {
    return source(input);
}

fn straight(rgb: vec3<f32>, alpha: f32) -> vec3<f32> {
    if alpha <= 0.000001 {
        return vec3<f32>(0.0);
    }
    return rgb / alpha;
}

fn composite_blend(input: VertexOutput, blended: vec3<f32>) -> vec4<f32> {
    let src = source(input);
    let dimensions = vec2<f32>(textureDimensions(backdrop_texture));
    let dst = textureSample(backdrop_texture, backdrop_sampler, input.position.xy / dimensions);
    let output_alpha = src.a + dst.a - src.a * dst.a;
    let output_rgb = (1.0 - dst.a) * src.rgb
        + (1.0 - src.a) * dst.rgb
        + src.a * dst.a * blended;
    return vec4<f32>(output_rgb, output_alpha);
}

@fragment fn fragment_multiply(input: VertexOutput) -> @location(0) vec4<f32> {
    let src = source(input);
    let dimensions = vec2<f32>(textureDimensions(backdrop_texture));
    let dst = textureSample(backdrop_texture, backdrop_sampler, input.position.xy / dimensions);
    return composite_blend(input, straight(dst.rgb, dst.a) * straight(src.rgb, src.a));
}

@fragment fn fragment_overlay(input: VertexOutput) -> @location(0) vec4<f32> {
    let src = source(input);
    let dimensions = vec2<f32>(textureDimensions(backdrop_texture));
    let dst = textureSample(backdrop_texture, backdrop_sampler, input.position.xy / dimensions);
    let cs = straight(src.rgb, src.a);
    let cb = straight(dst.rgb, dst.a);
    let low = 2.0 * cb * cs;
    let high = 1.0 - 2.0 * (1.0 - cb) * (1.0 - cs);
    return composite_blend(input, select(high, low, cb <= vec3<f32>(0.5)));
}

@fragment fn fragment_darken(input: VertexOutput) -> @location(0) vec4<f32> {
    let src = source(input);
    let dimensions = vec2<f32>(textureDimensions(backdrop_texture));
    let dst = textureSample(backdrop_texture, backdrop_sampler, input.position.xy / dimensions);
    return composite_blend(input, min(straight(dst.rgb, dst.a), straight(src.rgb, src.a)));
}

@fragment fn fragment_lighten(input: VertexOutput) -> @location(0) vec4<f32> {
    let src = source(input);
    let dimensions = vec2<f32>(textureDimensions(backdrop_texture));
    let dst = textureSample(backdrop_texture, backdrop_sampler, input.position.xy / dimensions);
    return composite_blend(input, max(straight(dst.rgb, dst.a), straight(src.rgb, src.a)));
}

@fragment fn fragment_difference(input: VertexOutput) -> @location(0) vec4<f32> {
    let src = source(input);
    let dimensions = vec2<f32>(textureDimensions(backdrop_texture));
    let dst = textureSample(backdrop_texture, backdrop_sampler, input.position.xy / dimensions);
    return composite_blend(input, abs(straight(dst.rgb, dst.a) - straight(src.rgb, src.a)));
}
";

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::mpsc};

    use anyhow::{Context, Result, ensure};

    use super::*;

    const TEST_WIDTH: u32 = 16;
    const TEST_HEIGHT: u32 = 16;
    const TEST_BYTES_PER_ROW: u32 = 256;

    struct TestGpu {
        device: wgpu::Device,
        queue: wgpu::Queue,
        renderer: GpuCanvasRenderer,
    }

    impl TestGpu {
        fn new() -> Result<Option<Self>> {
            let instance = wgpu::Instance::default();
            let adapter =
                match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                    ..Default::default()
                })) {
                    Ok(adapter) => adapter,
                    Err(wgpu::RequestAdapterError::NotFound { .. }) => return Ok(None),
                    Err(error) => {
                        return Err(error).context("find a GPU adapter for canvas conformance");
                    }
                };
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                    .context("create canvas conformance device")?;
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("canvas conformance texture layout"),
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
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("canvas conformance sampler"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            let renderer = GpuCanvasRenderer::new(&device, &queue, &layout, &sampler);
            Ok(Some(Self {
                device,
                queue,
                renderer,
            }))
        }

        fn render(&self, snapshot: &GraphicsSnapshot) -> Result<Vec<u8>> {
            let mut scene = self.renderer.compile(
                &self.device,
                &self.queue,
                snapshot,
                (TEST_WIDTH, TEST_HEIGHT, 1.0),
                None,
            )?;
            let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("canvas conformance readback"),
                size: u64::from(TEST_BYTES_PER_ROW) * u64::from(TEST_HEIGHT),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("canvas conformance encoder"),
                });
            ensure!(
                self.renderer.encode_if_dirty(&mut encoder, &mut scene)?,
                "new conformance scene was unexpectedly clean"
            );
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &scene.root.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(TEST_BYTES_PER_ROW),
                        rows_per_image: Some(TEST_HEIGHT),
                    },
                },
                wgpu::Extent3d {
                    width: TEST_WIDTH,
                    height: TEST_HEIGHT,
                    depth_or_array_layers: 1,
                },
            );
            self.queue.submit([encoder.finish()]);
            let slice = readback.slice(..);
            let (sender, receiver) = mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| {
                drop(sender.send(result));
            });
            self.device
                .poll(wgpu::PollType::wait_indefinitely())
                .context("wait for canvas conformance readback")?;
            receiver
                .recv()
                .context("canvas conformance callback dropped")?
                .context("map canvas conformance readback")?;
            let mapped = slice
                .get_mapped_range()
                .context("borrow canvas conformance readback")?;
            let mut pixels = Vec::with_capacity((TEST_WIDTH * TEST_HEIGHT * 4) as usize);
            for row in mapped.chunks_exact(TEST_BYTES_PER_ROW as usize) {
                let contents = row
                    .get(..(TEST_WIDTH * 4) as usize)
                    .context("canvas conformance row is too short")?;
                pixels.extend_from_slice(contents);
            }
            drop(mapped);
            readback.unmap();
            Ok(pixels)
        }
    }

    fn snapshot(commands: Vec<Command>) -> GraphicsSnapshot {
        GraphicsSnapshot {
            pane: 1,
            sequence: 1,
            packet: manifold_graphics::Packet { commands },
            images: BTreeMap::new(),
            text_layouts: BTreeMap::new(),
        }
    }

    fn pixel(pixels: &[u8], x: u32, y: u32) -> &[u8] {
        let offset = ((y * TEST_WIDTH + x) * 4) as usize;
        pixels.get(offset..offset + 4).unwrap_or_default()
    }

    fn assert_pixel_near(actual: &[u8], expected: [u8; 4]) {
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| i16::from(*actual).abs_diff(i16::from(expected)) <= 2),
            "expected {expected:?}, got {actual:?}"
        );
    }

    fn linear_to_srgb(value: f32) -> f32 {
        if value <= 0.003_130_8 {
            value * 12.92
        } else {
            1.055_f32.mul_add(value.powf(1.0 / 2.4), -0.055)
        }
    }

    fn blend_channels(
        backdrop: [f32; 3],
        source: [f32; 3],
        blend: impl Fn(f32, f32) -> f32,
    ) -> [f32; 3] {
        let [backdrop_red, backdrop_green, backdrop_blue] = backdrop;
        let [source_red, source_green, source_blue] = source;
        [
            blend(backdrop_red, source_red),
            blend(backdrop_green, source_green),
            blend(backdrop_blue, source_blue),
        ]
    }

    fn encoded_blend(mode: BlendMode, backdrop: Color, source: Color) -> [u8; 4] {
        let backdrop = [backdrop.r, backdrop.g, backdrop.b].map(srgb_to_linear);
        let source = [source.r, source.g, source.b].map(srgb_to_linear);
        let blended = match mode {
            BlendMode::SourceOver => source,
            BlendMode::Multiply => {
                blend_channels(backdrop, source, |backdrop, source| backdrop * source)
            }
            BlendMode::Screen => blend_channels(backdrop, source, |backdrop, source| {
                backdrop.mul_add(-source, backdrop + source)
            }),
            BlendMode::Overlay => blend_channels(backdrop, source, |backdrop, source| {
                if backdrop <= 0.5 {
                    2.0 * backdrop * source
                } else {
                    (2.0 * (1.0 - backdrop)).mul_add(-(1.0 - source), 1.0)
                }
            }),
            BlendMode::Darken => blend_channels(backdrop, source, f32::min),
            BlendMode::Lighten => blend_channels(backdrop, source, f32::max),
            BlendMode::Difference => blend_channels(backdrop, source, |backdrop, source| {
                (backdrop - source).abs()
            }),
            BlendMode::Plus => blend_channels(backdrop, source, |backdrop, source| {
                (backdrop + source).min(1.0)
            }),
        };
        let [red, green, blue] = blended.map(linear_to_srgb);
        [
            (red * 255.0).round() as u8,
            (green * 255.0).round() as u8,
            (blue * 255.0).round() as u8,
            255,
        ]
    }

    #[test]
    fn sequential_layers_reuse_targets_by_depth() -> Result<()> {
        let mut commands = Vec::new();
        for _ in 0..1_000 {
            commands.extend([Command::BeginLayer, Command::EndLayer]);
        }
        let snapshot = snapshot(commands);
        let mut compiler = SceneCompiler::new(TEST_WIDTH, TEST_HEIGHT, 1.0);
        compiler.compile(&snapshot)?;
        ensure!(
            compiler.layer_count == 1,
            "sequential layers allocated more than one target"
        );
        ensure!(
            compiler.ops.iter().all(|operation| match operation {
                CanvasOp::BeginLayer(layer) | CanvasOp::EndLayer { layer, .. } => *layer == 0,
                CanvasOp::Draw(_) => true,
            }),
            "sequential layer operation used a target above depth zero"
        );
        Ok(())
    }

    #[test]
    fn transforms_tighten_curve_tessellation_tolerance() {
        let compiler = SceneCompiler::new(TEST_WIDTH, TEST_HEIGHT, 2.0);
        let identity = compiler.tessellation_tolerance(Transform::IDENTITY);
        let scaled = compiler.tessellation_tolerance(Transform {
            sx: 20.0,
            sy: 20.0,
            ..Transform::IDENTITY
        });
        assert!(scaled < identity);
        assert!(scaled >= MIN_TESSELLATION_TOLERANCE);
    }

    #[test]
    fn scene_geometry_budget_is_hard() {
        let compiler = SceneCompiler::new(TEST_WIDTH, TEST_HEIGHT, 1.0);
        assert!(matches!(
            compiler.ensure_capacity(MAX_SCENE_VERTICES + 1, 0),
            Err(SceneCompileError::Limit)
        ));
        assert!(matches!(
            compiler.ensure_capacity(0, MAX_SCENE_INDICES + 1),
            Err(SceneCompileError::Limit)
        ));
    }

    #[test]
    fn gpu_canvas_conforms_for_clips_layers_blends_resources_and_antialiasing() -> Result<()> {
        let Some(gpu) = TestGpu::new()? else {
            eprintln!("skipping GPU canvas conformance: no graphics adapter is available");
            return Ok(());
        };
        let full = Rect::new(0.0, 0.0, TEST_WIDTH as f32, TEST_HEIGHT as f32);

        let clipped = gpu.render(&snapshot(vec![
            Command::FillRect {
                rect: full,
                color: Color::rgba(1.0, 0.0, 0.0, 1.0),
            },
            Command::Save,
            Command::ClipRect(Rect::new(0.0, 0.0, 8.0, 16.0)),
            Command::ClipRect(Rect::new(0.0, 0.0, 4.0, 16.0)),
            Command::FillRect {
                rect: full,
                color: Color::rgba(0.0, 0.0, 1.0, 1.0),
            },
            Command::Restore,
            Command::FillRect {
                rect: Rect::new(4.0, 0.0, 4.0, 16.0),
                color: Color::rgba(0.0, 1.0, 0.0, 1.0),
            },
            Command::BeginLayer,
            Command::BeginLayer,
            Command::FillRect {
                rect: Rect::new(8.0, 0.0, 4.0, 16.0),
                color: Color::rgba(1.0, 1.0, 0.0, 1.0),
            },
            Command::EndLayer,
            Command::EndLayer,
        ]))?;
        assert_pixel_near(pixel(&clipped, 2, 8), [0, 0, 255, 255]);
        assert_pixel_near(pixel(&clipped, 6, 8), [0, 255, 0, 255]);
        assert_pixel_near(pixel(&clipped, 10, 8), [255, 255, 0, 255]);
        assert_pixel_near(pixel(&clipped, 14, 8), [255, 0, 0, 255]);

        let backdrop = Color::rgba(0.2, 0.4, 0.6, 1.0);
        let source = Color::rgba(0.8, 0.5, 0.25, 1.0);
        for mode in [
            BlendMode::SourceOver,
            BlendMode::Multiply,
            BlendMode::Screen,
            BlendMode::Overlay,
            BlendMode::Darken,
            BlendMode::Lighten,
            BlendMode::Difference,
            BlendMode::Plus,
        ] {
            let pixels = gpu.render(&snapshot(vec![
                Command::FillRect {
                    rect: full,
                    color: backdrop,
                },
                Command::SetBlendMode(mode),
                Command::FillRect {
                    rect: full,
                    color: source,
                },
            ]))?;
            assert_pixel_near(pixel(&pixels, 8, 8), encoded_blend(mode, backdrop, source));
        }

        let image_handle = 7;
        let mut image_snapshot = snapshot(vec![Command::DrawImage {
            image: image_handle,
            destination: full,
        }]);
        image_snapshot.images.insert(
            image_handle,
            ImageResource {
                pane: 1,
                generation: 1,
                width: 2,
                height: 1,
                linear_premultiplied_rgba8: [0, 0, 0, 0, 255, 0, 0, 255].into(),
            },
        );
        let image = gpu.render(&image_snapshot)?;
        let transition = pixel(&image, 7, 8);
        let [red, green, blue, _alpha] =
            <[u8; 4]>::try_from(transition).context("image transition pixel is incomplete")?;
        ensure!(
            red > 0 && green == 0 && blue == 0,
            "transparent image edge gained a color fringe: {transition:?}"
        );

        let translucent = gpu.render(&snapshot(vec![Command::FillRect {
            rect: full,
            color: Color::rgba(1.0, 0.0, 0.0, 0.5),
        }]))?;
        assert_pixel_near(pixel(&translucent, 8, 8), [188, 0, 0, 128]);

        let nonzero = Path::new()
            .move_to(2.0, 2.0)
            .line_to(14.0, 2.0)
            .line_to(14.0, 14.0)
            .line_to(2.0, 14.0)
            .close()
            .move_to(5.0, 5.0)
            .line_to(11.0, 5.0)
            .line_to(11.0, 11.0)
            .line_to(5.0, 11.0)
            .close();
        let winding = gpu.render(&snapshot(vec![Command::FillPath {
            path: nonzero,
            color: Color::rgba(1.0, 1.0, 1.0, 1.0),
        }]))?;
        assert_pixel_near(pixel(&winding, 8, 8), [255, 255, 255, 255]);

        let diagonal = Path::new()
            .move_to(0.0, 0.0)
            .line_to(16.0, 16.0)
            .line_to(0.0, 16.0)
            .close();
        let antialiased = gpu.render(&snapshot(vec![Command::FillPath {
            path: diagonal,
            color: Color::rgba(1.0, 1.0, 1.0, 1.0),
        }]))?;
        ensure!(
            antialiased
                .chunks_exact(4)
                .filter_map(|pixel| pixel.last())
                .any(|alpha| *alpha > 0 && *alpha < 255),
            "diagonal path had no multisample edge coverage"
        );
        Ok(())
    }
}
