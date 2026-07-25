use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Sender},
    },
    thread,
    time::Instant,
};

use anyhow::{Result, anyhow};
use brood_graphics::{Command, Packet, PacketError};
use brood_wasm::{PresentationError, PresentationHost, TextLayoutOptions, TextLayoutResult};
use cosmic_text::{
    Attrs, Buffer, CacheKey, FontSystem, Metrics, Shaping, SwashCache, SwashContent,
};
use winit::event_loop::EventLoopProxy;

use crate::ClientEvent;

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESOURCE_DIMENSION: u32 = 4_096;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_TEXT_LAYOUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESOURCES_PER_PANE: usize = 1_024;
const MAX_RESOURCE_BYTES_PER_PANE: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct PluginGraphicsBridge {
    shared: Arc<Mutex<GraphicsState>>,
    text_layout: Arc<Mutex<FontSystem>>,
    glyph_raster_tx: Sender<GlyphRasterRequest>,
    proxy: EventLoopProxy<ClientEvent>,
}

impl fmt::Debug for PluginGraphicsBridge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginGraphicsBridge")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct ActorPresentationHost {
    pane: u64,
    generation: AtomicU64,
    bridge: PluginGraphicsBridge,
}

#[derive(Clone, Debug)]
pub struct GraphicsSnapshot {
    pub pane: u64,
    pub sequence: u64,
    pub packet: Packet,
    pub images: BTreeMap<u64, ImageResource>,
    pub text_layouts: BTreeMap<u64, TextLayoutResource>,
    pub glyphs: BTreeMap<GlyphCacheKey, GlyphImageResource>,
}

#[derive(Clone, Debug)]
pub struct ImageResource {
    pub pane: u64,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub linear_premultiplied_rgba8: Arc<[u8]>,
}

#[derive(Clone, Debug)]
struct PaneScene {
    generation: u64,
    sequence: u64,
    packet: Packet,
}

struct TextSystem {
    fonts: FontSystem,
    swash: SwashCache,
    glyphs: BTreeMap<GlyphCacheKey, Option<GlyphImageResource>>,
}

#[derive(Debug)]
struct GlyphRasterRequest {
    handle: u64,
    pane: u64,
    generation: u64,
    text: String,
    options: TextLayoutOptions,
    width: u32,
    height: u32,
}

#[derive(Clone, Debug)]
pub struct TextLayoutResource {
    pub width: u32,
    pub height: u32,
    pub glyphs: Arc<[GlyphSprite]>,
    pub glyph_images: BTreeMap<GlyphCacheKey, GlyphImageResource>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GlyphCacheKey {
    cache_key: CacheKey,
    color: [u8; 4],
}

#[derive(Clone, Debug)]
pub struct GlyphImageResource {
    pub width: u32,
    pub height: u32,
    pub linear_premultiplied_rgba8: Arc<[u8]>,
    left: i32,
    top: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct GlyphSprite {
    pub key: GlyphCacheKey,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug)]
struct TextLayoutEntry {
    pane: u64,
    generation: u64,
    width: u32,
    height: u32,
    byte_len: usize,
    state: TextLayoutState,
}

#[derive(Debug)]
enum TextLayoutState {
    Pending,
    Ready(TextLayoutResource),
    Failed,
}

#[derive(Debug, Default)]
struct GraphicsState {
    next_sequence: u64,
    next_resource: u64,
    panes: BTreeMap<u64, PaneScene>,
    images: BTreeMap<u64, ImageResource>,
    text_layouts: BTreeMap<u64, TextLayoutEntry>,
}

struct ReferencedResources {
    images: BTreeMap<u64, ImageResource>,
    text_layouts: BTreeMap<u64, TextLayoutResource>,
    glyphs: BTreeMap<GlyphCacheKey, GlyphImageResource>,
    ready: bool,
}

impl PluginGraphicsBridge {
    pub fn new(proxy: EventLoopProxy<ClientEvent>) -> Self {
        let shared = Arc::new(Mutex::new(GraphicsState::default()));
        let glyph_raster_tx = start_glyph_raster_worker(shared.clone(), proxy.clone());
        Self {
            shared,
            text_layout: Arc::new(Mutex::new(FontSystem::new())),
            glyph_raster_tx,
            proxy,
        }
    }

    pub fn actor_host(&self, pane: u64) -> Arc<ActorPresentationHost> {
        Arc::new(ActorPresentationHost {
            pane,
            generation: AtomicU64::new(0),
            bridge: self.clone(),
        })
    }

    pub fn snapshot(&self, pane: u64) -> Result<Option<GraphicsSnapshot>> {
        let state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        let Some(scene) = state.panes.get(&pane) else {
            return Ok(None);
        };
        let resources = referenced_resources(&state, pane, scene.generation, &scene.packet)
            .map_err(|_error| anyhow!("accepted packet lost a referenced resource"))?;
        if !resources.ready {
            return Ok(None);
        }
        let snapshot = GraphicsSnapshot {
            pane,
            sequence: scene.sequence,
            packet: scene.packet.clone(),
            images: resources.images,
            text_layouts: resources.text_layouts,
            glyphs: resources.glyphs,
        };
        drop(state);
        Ok(Some(snapshot))
    }
}

fn start_glyph_raster_worker(
    shared: Arc<Mutex<GraphicsState>>,
    proxy: EventLoopProxy<ClientEvent>,
) -> Sender<GlyphRasterRequest> {
    let (sender, receiver) = mpsc::channel::<GlyphRasterRequest>();
    let spawn_result = thread::Builder::new()
        .name("brood-glyph-raster".to_owned())
        .spawn(move || {
            let mut text_system = TextSystem {
                fonts: FontSystem::new(),
                swash: SwashCache::new(),
                glyphs: BTreeMap::new(),
            };
            while let Ok(request) = receiver.recv() {
                let started = Instant::now();
                let pane = request.pane;
                let handle = request.handle;
                let result =
                    create_text_layout_plan(&mut text_system.fonts, &request.text, request.options)
                        .and_then(|plan| {
                            if plan.width != request.width || plan.height != request.height {
                                return Err(PresentationError::HostFailure);
                            }
                            rasterize_text_layout(&mut text_system, plan, request.options)
                        });
                let elapsed = started.elapsed();
                finish_glyph_raster(&shared, &proxy, request, result);
                if std::env::var_os("BROOD_TRACE_PLUGIN_TEXT").is_some() || elapsed.as_millis() >= 8
                {
                    eprintln!(
                        "plugin glyph raster pane={pane} handle={handle} time_us={}",
                        elapsed.as_micros()
                    );
                }
            }
        });
    if let Err(error) = spawn_result {
        eprintln!("failed to start plugin glyph raster worker: {error}");
    }
    sender
}

fn finish_glyph_raster(
    shared: &Mutex<GraphicsState>,
    proxy: &EventLoopProxy<ClientEvent>,
    request: GlyphRasterRequest,
    result: Result<TextLayoutResource, PresentationError>,
) {
    let notification = {
        let Ok(mut state) = shared.lock() else {
            return;
        };
        let Some(entry) = state.text_layouts.get_mut(&request.handle) else {
            return;
        };
        if entry.pane != request.pane || entry.generation != request.generation {
            return;
        }
        match result {
            Ok(resource) if resource.width == entry.width && resource.height == entry.height => {
                entry.state = TextLayoutState::Ready(resource);
            }
            Ok(_) | Err(_) => {
                entry.state = TextLayoutState::Failed;
                eprintln!(
                    "plugin glyph raster failed pane={} handle={}",
                    request.pane, request.handle
                );
                return;
            }
        }
        let Some(scene) = state.panes.get(&request.pane) else {
            return;
        };
        if scene.generation != request.generation
            || !packet_references_text_layout(&scene.packet, request.handle)
        {
            return;
        }
        match referenced_resources(&state, request.pane, request.generation, &scene.packet) {
            Ok(resources) if resources.ready => Some(scene.sequence),
            Ok(_) | Err(_) => None,
        }
    };
    if let Some(sequence) = notification
        && proxy
            .send_event(ClientEvent::PluginGraphicsChanged {
                pane: request.pane,
                sequence,
            })
            .is_err()
    {
        eprintln!(
            "failed to publish ready plugin text scene pane={} sequence={sequence}",
            request.pane
        );
    }
}

fn packet_references_text_layout(packet: &Packet, handle: u64) -> bool {
    packet.commands.iter().any(
        |command| matches!(command, Command::DrawTextLayout { layout, .. } if *layout == handle),
    )
}

impl ActorPresentationHost {
    pub fn bind_generation(&self, generation: u64) {
        if let Ok(mut state) = self.bridge.shared.lock() {
            self.generation.store(generation, Ordering::Release);
            state
                .images
                .retain(|_, resource| resource.pane != self.pane);
            state
                .text_layouts
                .retain(|_, resource| resource.pane != self.pane);
            state.panes.remove(&self.pane);
        }
    }

    fn authorize(&self, actor_generation: u64, pane: u64) -> Result<(), PresentationError> {
        if pane != self.pane || actor_generation != self.generation.load(Ordering::Acquire) {
            return Err(PresentationError::InvalidPane);
        }
        Ok(())
    }
}

impl PresentationHost for ActorPresentationHost {
    fn submit_draw_packet(
        &self,
        actor_generation: u64,
        pane: u64,
        packet: Vec<u8>,
    ) -> Result<u64, PresentationError> {
        self.authorize(actor_generation, pane)?;
        let packet = Packet::decode(&packet).map_err(|error| match error {
            PacketError::Limit(_) => PresentationError::LimitReached,
            PacketError::Invalid(_) => PresentationError::InvalidPacket,
        })?;
        let resources = {
            let state = self
                .bridge
                .shared
                .lock()
                .map_err(|_poisoned| PresentationError::HostFailure)?;
            if actor_generation != self.generation.load(Ordering::Acquire) {
                return Err(PresentationError::InvalidPane);
            }
            referenced_resources(&state, pane, actor_generation, &packet)?
        };
        let candidate = GraphicsSnapshot {
            pane,
            sequence: 0,
            packet,
            images: resources.images,
            text_layouts: resources.text_layouts,
            glyphs: resources.glyphs,
        };
        crate::plugin_gpu::validate_snapshot_lowering(&candidate)?;
        let packet = candidate.packet;
        let (sequence, previous, ready) = {
            let mut state = self
                .bridge
                .shared
                .lock()
                .map_err(|_poisoned| PresentationError::HostFailure)?;
            if actor_generation != self.generation.load(Ordering::Acquire) {
                return Err(PresentationError::InvalidPane);
            }
            let resources = referenced_resources(&state, pane, actor_generation, &packet)?;
            state.next_sequence = state
                .next_sequence
                .checked_add(1)
                .ok_or(PresentationError::LimitReached)?;
            let sequence = state.next_sequence;
            let previous = state.panes.insert(
                pane,
                PaneScene {
                    generation: actor_generation,
                    sequence,
                    packet,
                },
            );
            drop(state);
            (sequence, previous, resources.ready)
        };
        if ready
            && self
                .bridge
                .proxy
                .send_event(ClientEvent::PluginGraphicsChanged { pane, sequence })
                .is_err()
        {
            if let Ok(mut state) = self.bridge.shared.lock() {
                match previous {
                    Some(previous) => {
                        state.panes.insert(pane, previous);
                    }
                    None => {
                        state.panes.remove(&pane);
                    }
                }
            }
            return Err(PresentationError::HostFailure);
        }
        Ok(sequence)
    }

    fn create_image_rgba8(
        &self,
        actor_generation: u64,
        pane: u64,
        width: u32,
        height: u32,
        mut pixels: Vec<u8>,
    ) -> Result<u64, PresentationError> {
        self.authorize(actor_generation, pane)?;
        if width == 0 || height == 0 {
            return Err(PresentationError::InvalidArgument);
        }
        if width > MAX_RESOURCE_DIMENSION || height > MAX_RESOURCE_DIMENSION {
            return Err(PresentationError::LimitReached);
        }
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(PresentationError::InvalidArgument)?;
        if expected != pixels.len() {
            return Err(PresentationError::InvalidArgument);
        }
        if expected > MAX_IMAGE_BYTES {
            return Err(PresentationError::LimitReached);
        }
        for pixel in pixels.chunks_exact_mut(4) {
            let Ok(pixel) = <&mut [u8; 4]>::try_from(pixel) else {
                return Err(PresentationError::HostFailure);
            };
            *pixel = linear_premultiplied_srgba8(*pixel);
        }
        let mut state = self
            .bridge
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if actor_generation != self.generation.load(Ordering::Acquire) {
            return Err(PresentationError::InvalidPane);
        }
        let handle = allocate_resource_handle(&mut state, pane, actor_generation, expected)?;
        state.images.insert(
            handle,
            ImageResource {
                pane,
                generation: actor_generation,
                width,
                height,
                linear_premultiplied_rgba8: pixels.into(),
            },
        );
        drop(state);
        Ok(handle)
    }

    fn layout_text(
        &self,
        actor_generation: u64,
        pane: u64,
        text: String,
        options: TextLayoutOptions,
    ) -> Result<TextLayoutResult, PresentationError> {
        self.authorize(actor_generation, pane)?;
        validate_text_request(&text, options)?;
        let started = Instant::now();
        let plan = {
            let mut fonts = self
                .bridge
                .text_layout
                .lock()
                .map_err(|_poisoned| PresentationError::HostFailure)?;
            create_text_layout_plan(&mut fonts, &text, options)?
        };
        let mut state = self
            .bridge
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if actor_generation != self.generation.load(Ordering::Acquire) {
            return Err(PresentationError::InvalidPane);
        }
        let handle = allocate_resource_handle(&mut state, pane, actor_generation, plan.byte_len)?;
        state.text_layouts.insert(
            handle,
            TextLayoutEntry {
                pane,
                generation: actor_generation,
                width: plan.width,
                height: plan.height,
                byte_len: plan.byte_len,
                state: TextLayoutState::Pending,
            },
        );
        drop(state);
        let request = GlyphRasterRequest {
            handle,
            pane,
            generation: actor_generation,
            text,
            options,
            width: plan.width,
            height: plan.height,
        };
        if self.bridge.glyph_raster_tx.send(request).is_err() {
            if let Ok(mut state) = self.bridge.shared.lock() {
                state.text_layouts.remove(&handle);
            }
            return Err(PresentationError::HostFailure);
        }
        let elapsed = started.elapsed();
        if std::env::var_os("BROOD_TRACE_PLUGIN_TEXT").is_some() || elapsed.as_millis() >= 8 {
            eprintln!(
                "plugin text layout pane={pane} handle={handle} bytes={} time_us={}",
                plan.byte_len,
                elapsed.as_micros(),
            );
        }
        Ok(TextLayoutResult {
            handle,
            ..plan.metrics
        })
    }

    fn drop_resource(
        &self,
        actor_generation: u64,
        pane: u64,
        resource: u64,
    ) -> Result<(), PresentationError> {
        self.authorize(actor_generation, pane)?;
        let mut state = self
            .bridge
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if actor_generation != self.generation.load(Ordering::Acquire) {
            return Err(PresentationError::InvalidPane);
        }
        if state.panes.values().any(|scene| {
            scene.packet.commands.iter().any(|command| match command {
                Command::DrawImage { image, .. } => *image == resource,
                Command::DrawTextLayout { layout, .. } => *layout == resource,
                _ => false,
            })
        }) {
            return Err(PresentationError::ResourceInUse);
        }
        let owned_image = state.images.get(&resource).is_some_and(|candidate| {
            candidate.pane == pane && candidate.generation == actor_generation
        });
        let owned_text = state.text_layouts.get(&resource).is_some_and(|candidate| {
            candidate.pane == pane && candidate.generation == actor_generation
        });
        if owned_image {
            state.images.remove(&resource);
        } else if owned_text {
            state.text_layouts.remove(&resource);
        } else {
            return Err(PresentationError::InvalidResource);
        }
        drop(state);
        Ok(())
    }
}

fn referenced_resources(
    state: &GraphicsState,
    pane: u64,
    generation: u64,
    packet: &Packet,
) -> Result<ReferencedResources, PresentationError> {
    let mut images = BTreeMap::new();
    let mut text_layouts = BTreeMap::new();
    let mut glyphs = BTreeMap::new();
    let mut ready = true;
    for command in &packet.commands {
        match command {
            Command::DrawImage { image, .. } => {
                let resource = state
                    .images
                    .get(image)
                    .filter(|resource| resource.pane == pane && resource.generation == generation)
                    .ok_or(PresentationError::InvalidResource)?;
                images.insert(*image, resource.clone());
            }
            Command::DrawTextLayout { layout, .. } => {
                let entry = state
                    .text_layouts
                    .get(layout)
                    .filter(|entry| entry.pane == pane && entry.generation == generation)
                    .ok_or(PresentationError::InvalidResource)?;
                let resource = match &entry.state {
                    TextLayoutState::Pending => {
                        ready = false;
                        TextLayoutResource {
                            width: entry.width,
                            height: entry.height,
                            glyphs: Arc::from([]),
                            glyph_images: BTreeMap::new(),
                        }
                    }
                    TextLayoutState::Ready(resource) => {
                        glyphs.extend(resource.glyph_images.clone());
                        resource.clone()
                    }
                    TextLayoutState::Failed => return Err(PresentationError::HostFailure),
                };
                text_layouts.insert(*layout, resource);
            }
            _ => {}
        }
    }
    Ok(ReferencedResources {
        images,
        text_layouts,
        glyphs,
        ready,
    })
}

impl Drop for ActorPresentationHost {
    fn drop(&mut self) {
        if let Ok(mut state) = self.bridge.shared.lock() {
            state
                .images
                .retain(|_, resource| resource.pane != self.pane);
            state
                .text_layouts
                .retain(|_, resource| resource.pane != self.pane);
            state.panes.remove(&self.pane);
        }
    }
}

fn allocate_resource_handle(
    state: &mut GraphicsState,
    pane: u64,
    generation: u64,
    new_bytes: usize,
) -> Result<u64, PresentationError> {
    let resource_count = state
        .images
        .values()
        .filter(|resource| resource.pane == pane && resource.generation == generation)
        .count()
        .saturating_add(
            state
                .text_layouts
                .values()
                .filter(|resource| resource.pane == pane && resource.generation == generation)
                .count(),
        );
    if resource_count >= MAX_RESOURCES_PER_PANE {
        return Err(PresentationError::LimitReached);
    }
    let retained_bytes = state
        .images
        .values()
        .filter(|resource| resource.pane == pane && resource.generation == generation)
        .map(|resource| resource.linear_premultiplied_rgba8.len())
        .chain(
            state
                .text_layouts
                .values()
                .filter(|resource| resource.pane == pane && resource.generation == generation)
                .map(|resource| resource.byte_len),
        )
        .try_fold(0_usize, usize::checked_add)
        .and_then(|bytes| bytes.checked_add(new_bytes))
        .ok_or(PresentationError::LimitReached)?;
    if retained_bytes > MAX_RESOURCE_BYTES_PER_PANE {
        return Err(PresentationError::LimitReached);
    }
    state.next_resource = state
        .next_resource
        .checked_add(1)
        .ok_or(PresentationError::LimitReached)?;
    Ok(state.next_resource)
}

fn validate_text_request(text: &str, options: TextLayoutOptions) -> Result<(), PresentationError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(PresentationError::LimitReached);
    }
    if !options.font_size.is_finite()
        || !(1.0..=512.0).contains(&options.font_size)
        || !options.max_width.is_finite()
        || !(1.0..=MAX_RESOURCE_DIMENSION as f32).contains(&options.max_width)
        || !options.line_height.is_finite()
        || options.line_height < options.font_size * 0.5
        || options.line_height > options.font_size * 4.0
        || !options
            .color
            .into_iter()
            .all(|channel| channel.is_finite() && (0.0..=1.0).contains(&channel))
    {
        return Err(PresentationError::InvalidArgument);
    }
    Ok(())
}

struct TextLayoutPlan {
    buffer: Buffer,
    metrics: TextLayoutResult,
    width: u32,
    height: u32,
    byte_len: usize,
}

fn create_text_layout_plan(
    fonts: &mut FontSystem,
    text: &str,
    options: TextLayoutOptions,
) -> Result<TextLayoutPlan, PresentationError> {
    let mut buffer = Buffer::new(fonts, Metrics::new(options.font_size, options.line_height));
    buffer.set_size(Some(options.max_width), None);
    buffer.set_text(text, &Attrs::new(), Shaping::Advanced, None);
    buffer.shape_until_scroll(fonts, false);

    let mut measured_width = 0.0_f32;
    let mut measured_height = 0.0_f32;
    let mut first_baseline = 0.0_f32;
    let mut line_count = 0_u32;
    for run in buffer.layout_runs() {
        if line_count == 0 {
            first_baseline = run.line_y;
        }
        measured_width = measured_width.max(run.line_w);
        measured_height = measured_height.max(run.line_top + run.line_height);
        line_count = line_count.saturating_add(1);
    }
    let width = measured_width.ceil().max(1.0) as u32;
    let height = measured_height.ceil().max(options.line_height).max(1.0) as u32;
    if width > MAX_RESOURCE_DIMENSION || height > MAX_RESOURCE_DIMENSION {
        return Err(PresentationError::LimitReached);
    }
    let bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(PresentationError::LimitReached)?;
    if bytes > MAX_TEXT_LAYOUT_BYTES {
        return Err(PresentationError::LimitReached);
    }
    Ok(TextLayoutPlan {
        buffer,
        metrics: TextLayoutResult {
            handle: 0,
            width: measured_width,
            height: measured_height.max(options.line_height),
            first_baseline,
            line_count,
        },
        width,
        height,
        byte_len: bytes,
    })
}

fn rasterize_text_layout(
    text_system: &mut TextSystem,
    plan: TextLayoutPlan,
    options: TextLayoutOptions,
) -> Result<TextLayoutResource, PresentationError> {
    let requested_color = options.color.map(unit_to_u8);
    let positioned = plan
        .buffer
        .layout_runs()
        .flat_map(|run| {
            run.glyphs.iter().map(move |glyph| {
                let physical = glyph.physical((0.0, run.line_y), 1.0);
                (physical.cache_key, physical.x, physical.y)
            })
        })
        .collect::<Vec<_>>();
    let mut sprites = Vec::with_capacity(positioned.len());
    let mut glyph_images = BTreeMap::new();
    for (cache_key, x, y) in positioned {
        let key = GlyphCacheKey {
            cache_key,
            color: requested_color,
        };
        if !text_system.glyphs.contains_key(&key) {
            let rendered = text_system
                .swash
                .get_image(&mut text_system.fonts, cache_key)
                .as_ref()
                .and_then(|image| glyph_image_resource(image, requested_color));
            text_system.glyphs.insert(key, rendered);
        }
        let Some(image) = text_system.glyphs.get(&key).cloned().flatten() else {
            continue;
        };
        sprites.push(GlyphSprite {
            key,
            x: x.saturating_add(image.left),
            y: y.saturating_sub(image.top),
        });
        glyph_images.insert(key, image);
    }
    Ok(TextLayoutResource {
        width: plan.width,
        height: plan.height,
        glyphs: sprites.into(),
        glyph_images,
    })
}

fn glyph_image_resource(
    image: &cosmic_text::SwashImage,
    requested_color: [u8; 4],
) -> Option<GlyphImageResource> {
    let width = image.placement.width;
    let height = image.placement.height;
    if width == 0 || height == 0 {
        return None;
    }
    let pixel_count = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?;
    let mut pixels = Vec::with_capacity(pixel_count.checked_mul(4)?);
    match image.content {
        SwashContent::Mask => {
            if image.data.len() != pixel_count {
                return None;
            }
            for coverage in &image.data {
                pixels.extend_from_slice(&premultiplied_glyph_color(requested_color, *coverage));
            }
        }
        SwashContent::Color => {
            if image.data.len() != pixel_count.checked_mul(4)? {
                return None;
            }
            for source in image.data.chunks_exact(4) {
                let source = <[u8; 4]>::try_from(source).ok()?;
                let opacity = unit_to_u8(
                    f32::from(source[3]) / 255.0 * f32::from(requested_color[3]) / 255.0,
                );
                pixels.extend_from_slice(&linear_premultiplied_srgba8([
                    source[0], source[1], source[2], opacity,
                ]));
            }
        }
        SwashContent::SubpixelMask => {
            if image.data.len() != pixel_count.checked_mul(4)? {
                return None;
            }
            for source in image.data.chunks_exact(4) {
                let coverage = source[..3].iter().copied().max().unwrap_or(0);
                pixels.extend_from_slice(&premultiplied_glyph_color(requested_color, coverage));
            }
        }
    }
    Some(GlyphImageResource {
        width,
        height,
        linear_premultiplied_rgba8: pixels.into(),
        left: image.placement.left,
        top: image.placement.top,
    })
}

fn premultiplied_glyph_color(color: [u8; 4], coverage: u8) -> [u8; 4] {
    let alpha = unit_to_u8(f32::from(color[3]) / 255.0 * f32::from(coverage) / 255.0);
    linear_premultiplied_srgba8([color[0], color[1], color[2], alpha])
}

fn unit_to_u8(value: f32) -> u8 {
    (value * 255.0).round().clamp(0.0, 255.0) as u8
}

fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_premultiplied_srgba8([red, green, blue, alpha]: [u8; 4]) -> [u8; 4] {
    let opacity = f32::from(alpha) / 255.0;
    [
        unit_to_u8(srgb_to_linear(f32::from(red) / 255.0) * opacity),
        unit_to_u8(srgb_to_linear(f32::from(green) / 255.0) * opacity),
        unit_to_u8(srgb_to_linear(f32::from(blue) / 255.0) * opacity),
        alpha,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_packet(handles: &[u64]) -> Packet {
        let mut packet = Packet::new();
        for handle in handles {
            packet.push(Command::DrawTextLayout {
                layout: *handle,
                x: 0.0,
                y: 0.0,
            });
        }
        packet
    }

    fn pending_text_entry() -> TextLayoutEntry {
        TextLayoutEntry {
            pane: 7,
            generation: 11,
            width: 1,
            height: 1,
            byte_len: 4,
            state: TextLayoutState::Pending,
        }
    }

    fn ready_text_layout() -> TextLayoutResource {
        TextLayoutResource {
            width: 1,
            height: 1,
            glyphs: Arc::from([]),
            glyph_images: BTreeMap::new(),
        }
    }

    #[test]
    fn pending_text_dependencies_hold_a_scene_until_all_are_ready() {
        let mut state = GraphicsState::default();
        state.text_layouts.insert(1, pending_text_entry());
        state.text_layouts.insert(2, pending_text_entry());
        let packet = text_packet(&[1, 2]);

        let pending =
            referenced_resources(&state, 7, 11, &packet).expect("pending handles resolve");
        assert!(!pending.ready);

        state.text_layouts.get_mut(&1).unwrap().state = TextLayoutState::Ready(ready_text_layout());
        let partly_ready =
            referenced_resources(&state, 7, 11, &packet).expect("ready handle resolves");
        assert!(!partly_ready.ready);

        state.text_layouts.get_mut(&2).unwrap().state = TextLayoutState::Ready(ready_text_layout());
        let ready = referenced_resources(&state, 7, 11, &packet).expect("all handles resolve");
        assert!(ready.ready);
        assert_eq!(ready.text_layouts.len(), 2);
        assert!(ready.glyphs.is_empty());
    }

    #[test]
    fn failed_text_dependency_rejects_the_scene() {
        let mut state = GraphicsState::default();
        let mut entry = pending_text_entry();
        entry.state = TextLayoutState::Failed;
        state.text_layouts.insert(1, entry);

        assert!(matches!(
            referenced_resources(&state, 7, 11, &text_packet(&[1])),
            Err(PresentationError::HostFailure)
        ));
    }
}
