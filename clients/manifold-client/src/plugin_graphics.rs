use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use cosmic_text::{Attrs, Buffer, Color as TextColor, FontSystem, Metrics, Shaping, SwashCache};
use manifold_graphics::{
    BlendMode, Color, Command, Packet, PacketError, Path, PathSegment, Rect, Transform,
};
use manifold_wasm::{PresentationError, PresentationHost, TextLayoutOptions, TextLayoutResult};
use tiny_skia::{
    FillRule, FilterQuality, LineCap, LineJoin, Mask, Paint, PathBuilder, Pixmap, PixmapPaint,
    Stroke,
};
use winit::event_loop::EventLoopProxy;

use crate::ClientEvent;

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_TEXT_LAYOUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESOURCES_PER_PANE: usize = 1_024;
const MAX_RESOURCE_BYTES_PER_PANE: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct PluginGraphicsBridge {
    shared: Arc<Mutex<GraphicsState>>,
    text: Arc<Mutex<TextSystem>>,
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
    images: BTreeMap<u64, ImageResource>,
    text_layouts: BTreeMap<u64, TextLayoutResource>,
}

#[derive(Clone, Debug)]
struct ImageResource {
    pane: u64,
    generation: u64,
    width: u32,
    height: u32,
    premultiplied_rgba: Vec<u8>,
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
}

#[derive(Clone, Debug)]
struct TextLayoutResource {
    pane: u64,
    generation: u64,
    width: u32,
    height: u32,
    premultiplied_rgba: Vec<u8>,
}

#[derive(Debug, Default)]
struct GraphicsState {
    next_sequence: u64,
    next_resource: u64,
    panes: BTreeMap<u64, PaneScene>,
    images: BTreeMap<u64, ImageResource>,
    text_layouts: BTreeMap<u64, TextLayoutResource>,
}

impl PluginGraphicsBridge {
    pub fn new(proxy: EventLoopProxy<ClientEvent>) -> Self {
        Self {
            shared: Arc::new(Mutex::new(GraphicsState::default())),
            text: Arc::new(Mutex::new(TextSystem {
                fonts: FontSystem::new(),
                swash: SwashCache::new(),
            })),
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
        let mut images = BTreeMap::new();
        let mut text_layouts = BTreeMap::new();
        for command in &scene.packet.commands {
            match command {
                Command::DrawImage { image, .. } => {
                    let resource = state
                        .images
                        .get(image)
                        .filter(|resource| {
                            resource.pane == pane && resource.generation == scene.generation
                        })
                        .with_context(|| format!("accepted packet lost image resource {image}"))?;
                    images.insert(*image, resource.clone());
                }
                Command::DrawTextLayout { layout, .. } => {
                    let resource = state
                        .text_layouts
                        .get(layout)
                        .filter(|resource| {
                            resource.pane == pane && resource.generation == scene.generation
                        })
                        .with_context(|| format!("accepted packet lost text layout {layout}"))?;
                    text_layouts.insert(*layout, resource.clone());
                }
                _ => {}
            }
        }
        let snapshot = GraphicsSnapshot {
            pane,
            sequence: scene.sequence,
            packet: scene.packet.clone(),
            images,
            text_layouts,
        };
        drop(state);
        Ok(Some(snapshot))
    }
}

impl ActorPresentationHost {
    pub fn bind_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Release);
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
        let (sequence, previous) = {
            let mut state = self
                .bridge
                .shared
                .lock()
                .map_err(|_poisoned| PresentationError::HostFailure)?;
            for command in &packet.commands {
                match command {
                    Command::DrawImage { image, .. }
                        if state.images.get(image).is_none_or(|resource| {
                            resource.pane != pane || resource.generation != actor_generation
                        }) =>
                    {
                        return Err(PresentationError::InvalidResource);
                    }
                    Command::DrawTextLayout { layout, .. }
                        if state.text_layouts.get(layout).is_none_or(|resource| {
                            resource.pane != pane || resource.generation != actor_generation
                        }) =>
                    {
                        return Err(PresentationError::InvalidResource);
                    }
                    _ => {}
                }
            }
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
            (sequence, previous)
        };
        if self
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
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(PresentationError::InvalidArgument)?;
        if width == 0 || height == 0 || expected != pixels.len() {
            return Err(PresentationError::InvalidArgument);
        }
        if expected > MAX_IMAGE_BYTES {
            return Err(PresentationError::LimitReached);
        }
        for pixel in pixels.chunks_exact_mut(4) {
            let Some((&mut alpha, channels)) = pixel.split_last_mut() else {
                return Err(PresentationError::HostFailure);
            };
            let alpha = u16::from(alpha);
            for channel in channels {
                *channel =
                    u8::try_from((u16::from(*channel) * alpha + 127) / 255).unwrap_or(u8::MAX);
            }
        }
        let mut state = self
            .bridge
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        let handle = allocate_resource_handle(&mut state, pane, actor_generation, expected)?;
        state.images.insert(
            handle,
            ImageResource {
                pane,
                generation: actor_generation,
                width,
                height,
                premultiplied_rgba: pixels,
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
        let (resource, metrics) = {
            let mut text_system = self
                .bridge
                .text
                .lock()
                .map_err(|_poisoned| PresentationError::HostFailure)?;
            create_text_layout(&mut text_system, &text, options)?
        };
        let mut state = self
            .bridge
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        let handle = allocate_resource_handle(
            &mut state,
            pane,
            actor_generation,
            resource.premultiplied_rgba.len(),
        )?;
        state.text_layouts.insert(
            handle,
            TextLayoutResource {
                pane,
                generation: actor_generation,
                ..resource
            },
        );
        drop(state);
        Ok(TextLayoutResult { handle, ..metrics })
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
        .map(|resource| resource.premultiplied_rgba.len())
        .chain(
            state
                .text_layouts
                .values()
                .filter(|resource| resource.pane == pane && resource.generation == generation)
                .map(|resource| resource.premultiplied_rgba.len()),
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
        || !(1.0..=16_384.0).contains(&options.max_width)
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

fn create_text_layout(
    text_system: &mut TextSystem,
    text: &str,
    options: TextLayoutOptions,
) -> Result<(TextLayoutResource, TextLayoutResult), PresentationError> {
    let mut buffer = Buffer::new(
        &mut text_system.fonts,
        Metrics::new(options.font_size, options.line_height),
    );
    buffer.set_size(Some(options.max_width), None);
    buffer.set_text(text, &Attrs::new(), Shaping::Advanced, None);
    buffer.shape_until_scroll(&mut text_system.fonts, false);

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
    let mut pixels = vec![0_u8; bytes];
    let [red, green, blue, alpha] = options.color;
    let text_color = TextColor::rgba(
        unit_to_u8(red),
        unit_to_u8(green),
        unit_to_u8(blue),
        unit_to_u8(alpha),
    );
    let TextSystem { fonts, swash } = text_system;
    buffer.draw(
        fonts,
        swash,
        text_color,
        |x, y, rect_width, rect_height, color| {
            for row in 0..rect_height {
                for column in 0..rect_width {
                    let pixel_x = x.saturating_add(i32::try_from(column).unwrap_or(i32::MAX));
                    let pixel_y = y.saturating_add(i32::try_from(row).unwrap_or(i32::MAX));
                    if pixel_x < 0 || pixel_y < 0 {
                        continue;
                    }
                    let Some(offset) = usize::try_from(pixel_y)
                        .ok()
                        .and_then(|pixel_y| pixel_y.checked_mul(width as usize))
                        .and_then(|row_offset| {
                            usize::try_from(pixel_x)
                                .ok()
                                .and_then(|pixel_x| row_offset.checked_add(pixel_x))
                        })
                        .filter(|pixel| *pixel < (width as usize).saturating_mul(height as usize))
                        .and_then(|pixel| pixel.checked_mul(4))
                    else {
                        continue;
                    };
                    let Some(destination) = pixels.get_mut(offset..offset.saturating_add(4)) else {
                        continue;
                    };
                    let Ok(destination) = <&mut [u8; 4]>::try_from(destination) else {
                        continue;
                    };
                    blend_text_pixel(destination, color);
                }
            }
        },
    );
    Ok((
        TextLayoutResource {
            pane: 0,
            generation: 0,
            width,
            height,
            premultiplied_rgba: pixels,
        },
        TextLayoutResult {
            handle: 0,
            width: measured_width,
            height: measured_height.max(options.line_height),
            first_baseline,
            line_count,
        },
    ))
}

fn unit_to_u8(value: f32) -> u8 {
    (value * 255.0).round().clamp(0.0, 255.0) as u8
}

fn blend_text_pixel(destination: &mut [u8; 4], color: TextColor) {
    let [red, green, blue, alpha] = color.as_rgba();
    let alpha_u16 = u16::from(alpha);
    let inverse = 255_u16.saturating_sub(alpha_u16);
    let premultiplied = [red, green, blue].map(|channel| {
        u8::try_from((u16::from(channel) * alpha_u16 + 127) / 255).unwrap_or(u8::MAX)
    });
    let [red, green, blue, output_alpha] = destination;
    for (output, source) in [red, green, blue].into_iter().zip(premultiplied) {
        *output = source.saturating_add(
            u8::try_from((u16::from(*output) * inverse + 127) / 255).unwrap_or(u8::MAX),
        );
    }
    *output_alpha = alpha.saturating_add(
        u8::try_from((u16::from(*output_alpha) * inverse + 127) / 255).unwrap_or(u8::MAX),
    );
}

#[derive(Clone)]
struct CanvasState {
    transform: Transform,
    opacity: f32,
    blend_mode: BlendMode,
    clip: Mask,
}

pub fn rasterize(
    snapshot: &GraphicsSnapshot,
    width: u32,
    height: u32,
    scale: f32,
) -> Result<Vec<u8>> {
    let mut targets = vec![Pixmap::new(width, height).context("allocate plugin pane pixmap")?];
    let mut clip = Mask::new(width, height).context("allocate plugin pane clip")?;
    let full = tiny_skia::Rect::from_xywh(0.0, 0.0, width as f32, height as f32)
        .context("create plugin pane bounds")?;
    clip.fill_path(
        &PathBuilder::from_rect(full),
        FillRule::Winding,
        false,
        tiny_skia::Transform::identity(),
    );
    let mut state = CanvasState {
        transform: Transform::IDENTITY,
        opacity: 1.0,
        blend_mode: BlendMode::SourceOver,
        clip,
    };
    let mut saved = Vec::new();

    for command in &snapshot.packet.commands {
        match command {
            Command::Save => saved.push(state.clone()),
            Command::Restore => {
                state = saved.pop().context("validated restore lost saved state")?;
            }
            Command::SetTransform(transform) => state.transform = *transform,
            Command::SetOpacity(opacity) => state.opacity = *opacity,
            Command::SetBlendMode(blend_mode) => state.blend_mode = *blend_mode,
            Command::ClipRect(rect) => {
                if let Some(path) = rect_path(*rect) {
                    state.clip.intersect_path(
                        &path,
                        FillRule::Winding,
                        true,
                        device_transform(state.transform, scale),
                    );
                } else {
                    state.clip.clear();
                }
            }
            Command::ClipPath(path) => {
                if let Some(path) = skia_path(path) {
                    state.clip.intersect_path(
                        &path,
                        FillRule::Winding,
                        true,
                        device_transform(state.transform, scale),
                    );
                } else {
                    state.clip.clear();
                }
            }
            Command::FillRect { rect, color } => {
                if let Some(path) = rect_path(*rect) {
                    draw_fill(
                        targets.last_mut().context("missing canvas target")?,
                        &path,
                        *color,
                        &state,
                        scale,
                    );
                }
            }
            Command::StrokeRect { rect, color, width } => {
                if let Some(path) = rect_path(*rect) {
                    draw_stroke(
                        targets.last_mut().context("missing canvas target")?,
                        &path,
                        *color,
                        *width,
                        &state,
                        scale,
                    );
                }
            }
            Command::FillRoundedRect {
                rect,
                radius,
                color,
            } => {
                if let Some(path) = rounded_rect_path(*rect, *radius) {
                    draw_fill(
                        targets.last_mut().context("missing canvas target")?,
                        &path,
                        *color,
                        &state,
                        scale,
                    );
                }
            }
            Command::StrokeRoundedRect {
                rect,
                radius,
                color,
                width,
            } => {
                if let Some(path) = rounded_rect_path(*rect, *radius) {
                    draw_stroke(
                        targets.last_mut().context("missing canvas target")?,
                        &path,
                        *color,
                        *width,
                        &state,
                        scale,
                    );
                }
            }
            Command::FillPath { path, color } => {
                if let Some(path) = skia_path(path) {
                    draw_fill(
                        targets.last_mut().context("missing canvas target")?,
                        &path,
                        *color,
                        &state,
                        scale,
                    );
                }
            }
            Command::StrokePath { path, color, width } => {
                if let Some(path) = skia_path(path) {
                    draw_stroke(
                        targets.last_mut().context("missing canvas target")?,
                        &path,
                        *color,
                        *width,
                        &state,
                        scale,
                    );
                }
            }
            Command::DrawImage { image, destination } => {
                if destination.width == 0.0 || destination.height == 0.0 {
                    continue;
                }
                let resource = snapshot
                    .images
                    .get(image)
                    .with_context(|| format!("missing retained image {image}"))?;
                let image = Pixmap::from_vec(
                    resource.premultiplied_rgba.clone(),
                    tiny_skia::IntSize::from_wh(resource.width, resource.height)
                        .context("invalid retained image size")?,
                )
                .context("invalid retained image pixels")?;
                let local = Transform {
                    sx: destination.width / resource.width as f32,
                    ky: 0.0,
                    kx: 0.0,
                    sy: destination.height / resource.height as f32,
                    tx: destination.x,
                    ty: destination.y,
                };
                let transform = concatenate(state.transform, local);
                targets
                    .last_mut()
                    .context("missing canvas target")?
                    .draw_pixmap(
                        0,
                        0,
                        image.as_ref(),
                        &PixmapPaint {
                            opacity: state.opacity,
                            blend_mode: skia_blend(state.blend_mode),
                            quality: FilterQuality::Bilinear,
                        },
                        device_transform(transform, scale),
                        Some(&state.clip),
                    );
            }
            Command::DrawTextLayout { layout, x, y } => {
                let resource = snapshot
                    .text_layouts
                    .get(layout)
                    .with_context(|| format!("missing retained text layout {layout}"))?;
                let text = Pixmap::from_vec(
                    resource.premultiplied_rgba.clone(),
                    tiny_skia::IntSize::from_wh(resource.width, resource.height)
                        .context("invalid retained text layout size")?,
                )
                .context("invalid retained text layout pixels")?;
                let transform = concatenate(
                    state.transform,
                    Transform {
                        tx: *x,
                        ty: *y,
                        ..Transform::IDENTITY
                    },
                );
                targets
                    .last_mut()
                    .context("missing canvas target")?
                    .draw_pixmap(
                        0,
                        0,
                        text.as_ref(),
                        &PixmapPaint {
                            opacity: state.opacity,
                            blend_mode: skia_blend(state.blend_mode),
                            quality: FilterQuality::Bilinear,
                        },
                        device_transform(transform, scale),
                        Some(&state.clip),
                    );
            }
            Command::BeginLayer => {
                targets.push(Pixmap::new(width, height).context("allocate canvas layer")?);
            }
            Command::EndLayer => {
                let layer = targets.pop().context("validated layer stack underflowed")?;
                targets
                    .last_mut()
                    .context("validated layer lost parent")?
                    .draw_pixmap(
                        0,
                        0,
                        layer.as_ref(),
                        &PixmapPaint::default(),
                        tiny_skia::Transform::identity(),
                        Some(&state.clip),
                    );
            }
        }
    }
    Ok(targets
        .pop()
        .context("canvas root target disappeared")?
        .take())
}

fn draw_fill(
    target: &mut Pixmap,
    path: &tiny_skia::Path,
    color: Color,
    state: &CanvasState,
    scale: f32,
) {
    target.fill_path(
        path,
        &paint(color, state),
        FillRule::Winding,
        device_transform(state.transform, scale),
        Some(&state.clip),
    );
}

fn draw_stroke(
    target: &mut Pixmap,
    path: &tiny_skia::Path,
    color: Color,
    width: f32,
    state: &CanvasState,
    scale: f32,
) {
    target.stroke_path(
        path,
        &paint(color, state),
        &Stroke {
            width,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Stroke::default()
        },
        device_transform(state.transform, scale),
        Some(&state.clip),
    );
}

fn paint(color: Color, state: &CanvasState) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(
        tiny_skia::Color::from_rgba(color.r, color.g, color.b, color.a * state.opacity)
            .unwrap_or(tiny_skia::Color::TRANSPARENT),
    );
    paint.blend_mode = skia_blend(state.blend_mode);
    paint
}

const fn skia_blend(mode: BlendMode) -> tiny_skia::BlendMode {
    match mode {
        BlendMode::SourceOver => tiny_skia::BlendMode::SourceOver,
        BlendMode::Multiply => tiny_skia::BlendMode::Multiply,
        BlendMode::Screen => tiny_skia::BlendMode::Screen,
        BlendMode::Overlay => tiny_skia::BlendMode::Overlay,
        BlendMode::Darken => tiny_skia::BlendMode::Darken,
        BlendMode::Lighten => tiny_skia::BlendMode::Lighten,
        BlendMode::Difference => tiny_skia::BlendMode::Difference,
        BlendMode::Plus => tiny_skia::BlendMode::Plus,
    }
}

fn device_transform(transform: Transform, scale: f32) -> tiny_skia::Transform {
    tiny_skia::Transform::from_row(
        transform.sx * scale,
        transform.ky * scale,
        transform.kx * scale,
        transform.sy * scale,
        transform.tx * scale,
        transform.ty * scale,
    )
}

fn concatenate(parent: Transform, local: Transform) -> Transform {
    Transform {
        sx: parent.sx.mul_add(local.sx, parent.kx * local.ky),
        ky: parent.ky.mul_add(local.sx, parent.sy * local.ky),
        kx: parent.sx.mul_add(local.kx, parent.kx * local.sy),
        sy: parent.ky.mul_add(local.kx, parent.sy * local.sy),
        tx: parent
            .sx
            .mul_add(local.tx, parent.kx.mul_add(local.ty, parent.tx)),
        ty: parent
            .ky
            .mul_add(local.tx, parent.sy.mul_add(local.ty, parent.ty)),
    }
}

fn rect_path(rect: Rect) -> Option<tiny_skia::Path> {
    tiny_skia::Rect::from_xywh(rect.x, rect.y, rect.width, rect.height).map(PathBuilder::from_rect)
}

fn rounded_rect_path(rect: Rect, radius: f32) -> Option<tiny_skia::Path> {
    let radius = radius.min(rect.width * 0.5).min(rect.height * 0.5);
    if radius <= f32::EPSILON {
        return rect_path(rect);
    }
    let mut builder = PathBuilder::new();
    let right = rect.x + rect.width;
    let bottom = rect.y + rect.height;
    let control = radius * 0.552_284_8;
    builder.move_to(rect.x + radius, rect.y);
    builder.line_to(right - radius, rect.y);
    builder.cubic_to(
        right - radius + control,
        rect.y,
        right,
        rect.y + radius - control,
        right,
        rect.y + radius,
    );
    builder.line_to(right, bottom - radius);
    builder.cubic_to(
        right,
        bottom - radius + control,
        right - radius + control,
        bottom,
        right - radius,
        bottom,
    );
    builder.line_to(rect.x + radius, bottom);
    builder.cubic_to(
        rect.x + radius - control,
        bottom,
        rect.x,
        bottom - radius + control,
        rect.x,
        bottom - radius,
    );
    builder.line_to(rect.x, rect.y + radius);
    builder.cubic_to(
        rect.x,
        rect.y + radius - control,
        rect.x + radius - control,
        rect.y,
        rect.x + radius,
        rect.y,
    );
    builder.close();
    builder.finish()
}

fn skia_path(path: &Path) -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();
    for segment in &path.segments {
        match *segment {
            PathSegment::MoveTo(x, y) => builder.move_to(x, y),
            PathSegment::LineTo(x, y) => builder.line_to(x, y),
            PathSegment::QuadTo(x1, y1, x, y) => builder.quad_to(x1, y1, x, y),
            PathSegment::CubicTo(x1, y1, x2, y2, x, y) => {
                builder.cubic_to(x1, y1, x2, y2, x, y);
            }
            PathSegment::Close => builder.close(),
        }
    }
    builder.finish()
}
