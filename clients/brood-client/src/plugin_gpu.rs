use std::{
    collections::BTreeMap,
    mem,
    ops::Range,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use brood_graphics::{BlendMode, Color, Command, Path, PathSegment, Rect, Transform};
use brood_wasm::PresentationError;
use bytemuck::{Pod, Zeroable};
use lyon_tessellation::{
    FillGeometryBuilder, FillOptions, FillRule, FillTessellator, FillVertex, GeometryBuilder,
    GeometryBuilderError, LineCap, LineJoin, StrokeGeometryBuilder, StrokeOptions,
    StrokeTessellator, StrokeVertex, TessellationError, VertexBuffers, VertexId,
    math::{Point, point},
    path::Path as LyonPath,
};
use thiserror::Error;
use wgpu::util::DeviceExt;

use crate::plugin_graphics::{GlyphCacheKey, GlyphImageResource, GraphicsSnapshot, ImageResource};

const CANVAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const RESOURCE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const STENCIL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24PlusStencil8;
const SAMPLE_COUNT: u32 = 4;
const SLOW_FRAME: Duration = Duration::from_millis(8);
const GLYPH_ATLAS_PAGE_SIZE: u32 = 1_024;
const GLYPH_ATLAS_PADDING: u32 = 1;
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
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct Geometry {
    indices: Range<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DrawSource {
    White,
    Resource(u64),
    GlyphAtlas(usize),
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
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

#[derive(Clone, Copy, Debug)]
struct GlyphAtlasEntry {
    page: usize,
    uv: [f32; 4],
}

#[derive(Debug, Default)]
struct GlyphAtlasPageAllocator {
    next_x: u32,
    next_y: u32,
    row_height: u32,
}

struct GlyphAtlasPage {
    texture: GpuTexture,
    allocator: GlyphAtlasPageAllocator,
}

#[derive(Default)]
struct GpuGlyphAtlas {
    pages: Vec<GlyphAtlasPage>,
    entries: BTreeMap<GlyphCacheKey, GlyphAtlasEntry>,
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
    pub scale: f32,
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

/// CPU-side lowering produced away from Winit's latency-critical thread.
///
/// GPU allocation and resource reuse remain on the renderer thread, but path
/// tessellation, command validation, and draw-list construction happen before
/// this value is returned by the bounded graphics-preparation lane.
#[derive(Debug)]
pub struct PreparedCanvasScene {
    snapshot: GraphicsSnapshot,
    width: u32,
    height: u32,
    scale: f32,
    vertices: Vec<CanvasVertex>,
    indices: Vec<u32>,
    ops: Vec<CanvasOp>,
    layer_count: usize,
    preparation_us: u128,
}

#[derive(Clone)]
pub struct GpuCanvasPreparationContext {
    glyph_atlas: BTreeMap<GlyphCacheKey, GlyphAtlasEntry>,
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
    glyph_atlas: GpuGlyphAtlas,
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
        .compile(snapshot, None)
        .map_err(|error| error.presentation_error())
}

impl GlyphAtlasPageAllocator {
    fn allocate(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        if width == 0
            || height == 0
            || width > GLYPH_ATLAS_PAGE_SIZE
            || height > GLYPH_ATLAS_PAGE_SIZE
        {
            return None;
        }
        if self.next_x.checked_add(width)? > GLYPH_ATLAS_PAGE_SIZE {
            self.next_x = 0;
            self.next_y = self.next_y.checked_add(self.row_height)?;
            self.row_height = 0;
        }
        if self.next_y.checked_add(height)? > GLYPH_ATLAS_PAGE_SIZE {
            return None;
        }
        let origin = (self.next_x, self.next_y);
        self.next_x = self.next_x.checked_add(width)?;
        self.row_height = self.row_height.max(height);
        Some(origin)
    }
}

impl GpuGlyphAtlas {
    fn ensure(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        key: GlyphCacheKey,
        resource: &GlyphImageResource,
    ) -> Result<()> {
        if self.entries.contains_key(&key) {
            return Ok(());
        }
        let padded_width = resource
            .width
            .checked_add(GLYPH_ATLAS_PADDING.saturating_mul(2))
            .context("glyph atlas width overflow")?;
        let padded_height = resource
            .height
            .checked_add(GLYPH_ATLAS_PADDING.saturating_mul(2))
            .context("glyph atlas height overflow")?;
        let allocation = self.pages.iter_mut().enumerate().find_map(|(page, state)| {
            state
                .allocator
                .allocate(padded_width, padded_height)
                .map(|origin| (page, origin))
        });
        let (page, (outer_x, outer_y)) = if let Some(allocation) = allocation {
            allocation
        } else {
            let page = self.pages.len();
            let mut atlas_page = GlyphAtlasPage {
                texture: create_empty_texture(
                    device,
                    layout,
                    sampler,
                    (GLYPH_ATLAS_PAGE_SIZE, GLYPH_ATLAS_PAGE_SIZE),
                    &format!("plugin GPU glyph atlas page {page}"),
                ),
                allocator: GlyphAtlasPageAllocator::default(),
            };
            let origin = atlas_page
                .allocator
                .allocate(padded_width, padded_height)
                .ok_or_else(|| anyhow!("glyph is too large for an atlas page"))?;
            self.pages.push(atlas_page);
            (page, origin)
        };
        let padded_bytes = usize::try_from(padded_width)
            .ok()
            .and_then(|width| {
                usize::try_from(padded_height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .context("glyph atlas upload size overflow")?;
        let mut pixels = vec![0_u8; padded_bytes];
        let source_stride = usize::try_from(resource.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .context("glyph source stride overflow")?;
        let destination_stride = usize::try_from(padded_width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .context("glyph atlas stride overflow")?;
        for row in 0..usize::try_from(resource.height).context("glyph height overflow")? {
            let source_start = row
                .checked_mul(source_stride)
                .context("glyph source offset overflow")?;
            let destination_start = row
                .checked_add(GLYPH_ATLAS_PADDING as usize)
                .and_then(|row| row.checked_mul(destination_stride))
                .and_then(|offset| offset.checked_add(GLYPH_ATLAS_PADDING as usize * 4))
                .context("glyph atlas offset overflow")?;
            let source_end = source_start
                .checked_add(source_stride)
                .context("glyph source range overflow")?;
            let destination_end = destination_start
                .checked_add(source_stride)
                .context("glyph atlas range overflow")?;
            let source = resource
                .linear_premultiplied_rgba8
                .get(source_start..source_end)
                .context("glyph source row is out of bounds")?;
            let destination = pixels
                .get_mut(destination_start..destination_end)
                .context("glyph atlas row is out of bounds")?;
            destination.copy_from_slice(source);
        }
        let page_texture = &self
            .pages
            .get(page)
            .context("allocated glyph atlas page is missing")?
            .texture
            .texture;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: page_texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: outer_x,
                    y: outer_y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_width.saturating_mul(4)),
                rows_per_image: Some(padded_height),
            },
            wgpu::Extent3d {
                width: padded_width,
                height: padded_height,
                depth_or_array_layers: 1,
            },
        );
        let x = outer_x.saturating_add(GLYPH_ATLAS_PADDING);
        let y = outer_y.saturating_add(GLYPH_ATLAS_PADDING);
        let atlas_size = GLYPH_ATLAS_PAGE_SIZE as f32;
        self.entries.insert(
            key,
            GlyphAtlasEntry {
                page,
                uv: [
                    x as f32 / atlas_size,
                    y as f32 / atlas_size,
                    x.saturating_add(resource.width) as f32 / atlas_size,
                    y.saturating_add(resource.height) as f32 / atlas_size,
                ],
            },
        );
        Ok(())
    }

    fn bind_group(&self, page: usize) -> Option<&wgpu::BindGroup> {
        self.pages.get(page).map(|page| &page.texture.bind_group)
    }
}

impl GpuCanvasRenderer {
    pub fn preparation_context(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        snapshot: &GraphicsSnapshot,
    ) -> Result<GpuCanvasPreparationContext> {
        for (key, resource) in &snapshot.glyphs {
            self.glyph_atlas.ensure(
                device,
                queue,
                &self.texture_layout,
                &self.sampler,
                *key,
                resource,
            )?;
        }
        Ok(GpuCanvasPreparationContext {
            glyph_atlas: self.glyph_atlas.entries.clone(),
        })
    }

    pub fn prepare(
        snapshot: GraphicsSnapshot,
        extent: (u32, u32, f32),
        context: &GpuCanvasPreparationContext,
    ) -> Result<PreparedCanvasScene> {
        let started = Instant::now();
        let (width, height, scale) = extent;
        let mut compiler = SceneCompiler::new(width, height, scale);
        compiler.compile(&snapshot, Some(&context.glyph_atlas))?;
        let SceneCompiler {
            vertices,
            indices,
            ops,
            layer_count,
            ..
        } = compiler;
        Ok(PreparedCanvasScene {
            snapshot,
            width,
            height,
            scale,
            vertices,
            indices,
            ops,
            layer_count,
            preparation_us: started.elapsed().as_micros(),
        })
    }

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
            glyph_atlas: GpuGlyphAtlas::default(),
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

    pub fn finish_prepared(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        prepared: PreparedCanvasScene,
        previous: Option<&GpuCanvasScene>,
    ) -> GpuCanvasScene {
        let started = Instant::now();
        let PreparedCanvasScene {
            snapshot,
            width,
            height,
            scale,
            vertices,
            indices,
            ops,
            layer_count,
            preparation_us,
        } = prepared;
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
                "compiled plugin GPU scene pane={} sequence={} commands={} draws={} vertices={} indices={} layers={} glyph_atlas_pages={} cached_glyphs={} preparation_us={} gpu_finish_us={}",
                snapshot.pane,
                snapshot.sequence,
                snapshot.packet.commands.len(),
                ops.iter()
                    .filter(|op| !matches!(op, CanvasOp::BeginLayer(_)))
                    .count(),
                vertices.len(),
                indices.len(),
                layer_count,
                self.glyph_atlas.pages.len(),
                self.glyph_atlas.entries.len(),
                preparation_us,
                elapsed.as_micros(),
            );
        }
        GpuCanvasScene {
            pane: snapshot.pane,
            sequence: snapshot.sequence,
            width,
            height,
            scale,
            vertex_buffer,
            index_buffer,
            ops,
            resources,
            root,
            layers,
            scratch,
            stencil,
            dirty: true,
        }
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
        let simple = scene.ops.iter().all(|op| {
            matches!(
                op,
                CanvasOp::Draw(draw)
                    if draw.clips.is_empty() && !is_advanced_blend(draw.blend_mode)
            )
        });
        if simple {
            self.encode_simple_scene(encoder, scene)?;
        } else {
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

    fn encode_simple_scene(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        scene: &GpuCanvasScene,
    ) -> Result<()> {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("plugin GPU canvas simple scene"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &scene.root.multisample_view,
                resolve_target: Some(&scene.root.view),
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
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
        pass.set_stencil_reference(0);
        for op in &scene.ops {
            let CanvasOp::Draw(draw) = op else {
                return Err(anyhow!("simple plugin scene contains a layer"));
            };
            pass.set_pipeline(self.pipeline(draw.blend_mode));
            pass.set_bind_group(0, self.source_bind_group(scene, &draw.source)?, &[]);
            pass.draw_indexed(draw.geometry.indices.clone(), 0, 0..1);
        }
        drop(pass);
        Ok(())
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
            DrawSource::GlyphAtlas(page) => self
                .glyph_atlas
                .bind_group(*page)
                .with_context(|| format!("plugin GPU glyph atlas page {page} disappeared")),
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

    fn compile(
        &mut self,
        snapshot: &GraphicsSnapshot,
        glyph_atlas: Option<&BTreeMap<GlyphCacheKey, GlyphAtlasEntry>>,
    ) -> CompileResult<()> {
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
                    for glyph in resource.glyphs.iter() {
                        let image = snapshot.glyphs.get(&glyph.key).ok_or_else(|| {
                            SceneCompileError::Invalid(format!(
                                "text layout {layout} glyph {:?} is unavailable",
                                glyph.key
                            ))
                        })?;
                        let atlas = match glyph_atlas {
                            Some(entries) => entries.get(&glyph.key).copied().ok_or_else(|| {
                                SceneCompileError::Invalid(format!(
                                    "text layout {layout} glyph {:?} is absent from the atlas",
                                    glyph.key
                                ))
                            })?,
                            None => GlyphAtlasEntry {
                                page: 0,
                                uv: [0.0, 0.0, 1.0, 1.0],
                            },
                        };
                        let geometry = self.quad_geometry_with_uv(
                            Rect::new(
                                *x + glyph.x,
                                *y + glyph.y,
                                image.width as f32 / resource.raster_scale,
                                image.height as f32 / resource.raster_scale,
                            ),
                            state.transform,
                            [state.opacity; 4],
                            atlas.uv,
                        )?;
                        self.push_draw(geometry, DrawSource::GlyphAtlas(atlas.page), &state);
                    }
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
        if let Some(CanvasOp::Draw(previous)) = self.ops.last_mut()
            && !is_advanced_blend(state.blend_mode)
            && previous.source == source
            && previous.blend_mode == state.blend_mode
            && previous.clips == state.clips
            && previous.geometry.indices.end == geometry.indices.start
        {
            previous.geometry.indices.end = geometry.indices.end;
            return;
        }
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
        let (top_uv, bottom_uv) = if flip_vertical {
            (1.0, 0.0)
        } else {
            (0.0, 1.0)
        };
        self.quad_geometry_with_uv(rect, transform, color, [0.0, top_uv, 1.0, bottom_uv])
    }

    fn quad_geometry_with_uv(
        &mut self,
        rect: Rect,
        transform: Transform,
        color: [f32; 4],
        [left_uv, top_uv, right_uv, bottom_uv]: [f32; 4],
    ) -> CompileResult<Geometry> {
        self.ensure_capacity(4, 6)?;
        let base_vertex =
            u32::try_from(self.vertices.len()).map_err(|_overflow| SceneCompileError::Limit)?;
        let first_index =
            u32::try_from(self.indices.len()).map_err(|_overflow| SceneCompileError::Limit)?;
        let right = rect.x + rect.width;
        let bottom = rect.y + rect.height;
        for ((x, y), uv) in [
            ((rect.x, rect.y), [left_uv, top_uv]),
            ((right, rect.y), [right_uv, top_uv]),
            ((right, bottom), [right_uv, bottom_uv]),
            ((rect.x, bottom), [left_uv, bottom_uv]),
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
    let texture = create_empty_texture(device, layout, sampler, extent, label);
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture.texture,
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
    texture
}

fn create_empty_texture(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    extent: (u32, u32),
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
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = texture_bind_group(device, layout, sampler, &view, label);
    GpuTexture {
        texture,
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
    use super::*;

    #[test]
    fn glyph_atlas_allocator_wraps_rows_without_overlap() {
        let mut allocator = GlyphAtlasPageAllocator::default();
        assert_eq!(allocator.allocate(600, 10), Some((0, 0)));
        assert_eq!(allocator.allocate(600, 20), Some((0, 10)));
        assert_eq!(allocator.allocate(400, 5), Some((600, 10)));
        assert_eq!(allocator.allocate(GLYPH_ATLAS_PAGE_SIZE + 1, 1), None);
    }

    #[test]
    fn adjacent_atlas_glyphs_share_one_draw() -> Result<()> {
        let mut compiler = SceneCompiler::new(100, 100, 1.0);
        let state = CompileState::default();
        for x in [0.0, 10.0] {
            let geometry = compiler.quad_geometry_with_uv(
                Rect::new(x, 0.0, 8.0, 12.0),
                Transform::IDENTITY,
                [1.0; 4],
                [0.0, 0.0, 0.1, 0.1],
            )?;
            compiler.push_draw(geometry, DrawSource::GlyphAtlas(0), &state);
        }
        anyhow::ensure!(compiler.ops.len() == 1, "adjacent glyphs did not merge");
        let Some(CanvasOp::Draw(draw)) = compiler.ops.first() else {
            return Err(anyhow!("expected a merged draw"));
        };
        anyhow::ensure!(
            draw.geometry.indices == (0..12),
            "merged draw has the wrong index range"
        );

        let geometry = compiler.quad_geometry_with_uv(
            Rect::new(20.0, 0.0, 8.0, 12.0),
            Transform::IDENTITY,
            [1.0; 4],
            [0.0, 0.0, 0.1, 0.1],
        )?;
        compiler.push_draw(geometry, DrawSource::GlyphAtlas(1), &state);
        anyhow::ensure!(
            compiler.ops.len() == 2,
            "glyphs from different atlas pages merged"
        );
        Ok(())
    }

    #[test]
    fn advanced_blend_draws_are_not_coalesced() -> Result<()> {
        let mut compiler = SceneCompiler::new(100, 100, 1.0);
        let state = CompileState {
            blend_mode: BlendMode::Multiply,
            ..CompileState::default()
        };
        for x in [0.0, 10.0] {
            let geometry = compiler.quad_geometry_with_uv(
                Rect::new(x, 0.0, 8.0, 12.0),
                Transform::IDENTITY,
                [1.0; 4],
                [0.0, 0.0, 0.1, 0.1],
            )?;
            compiler.push_draw(geometry, DrawSource::GlyphAtlas(0), &state);
        }
        anyhow::ensure!(
            compiler.ops.len() == 2,
            "advanced blend draws were coalesced"
        );
        Ok(())
    }
}
