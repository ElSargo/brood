use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Result, anyhow};
use cosmic_text::{Attrs, Buffer, Color as TextColor, FontSystem, Metrics, Shaping, SwashCache};
use manifold_graphics::{Command, Packet, PacketError};
use manifold_wasm::{PresentationError, PresentationHost, TextLayoutOptions, TextLayoutResult};
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
    pub images: BTreeMap<u64, ImageResource>,
    pub text_layouts: BTreeMap<u64, TextLayoutResource>,
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
}

#[derive(Clone, Debug)]
pub struct TextLayoutResource {
    pub pane: u64,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub linear_premultiplied_rgba8: Arc<[u8]>,
}

#[derive(Debug, Default)]
struct GraphicsState {
    next_sequence: u64,
    next_resource: u64,
    panes: BTreeMap<u64, PaneScene>,
    images: BTreeMap<u64, ImageResource>,
    text_layouts: BTreeMap<u64, TextLayoutResource>,
}

type ReferencedResources = (
    BTreeMap<u64, ImageResource>,
    BTreeMap<u64, TextLayoutResource>,
);

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
        let (images, text_layouts) =
            referenced_resources(&state, pane, scene.generation, &scene.packet)
                .map_err(|_error| anyhow!("accepted packet lost a referenced resource"))?;
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
        let (images, text_layouts) = {
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
            images,
            text_layouts,
        };
        crate::plugin_gpu::validate_snapshot_lowering(&candidate)?;
        let packet = candidate.packet;
        let (sequence, previous) = {
            let mut state = self
                .bridge
                .shared
                .lock()
                .map_err(|_poisoned| PresentationError::HostFailure)?;
            if actor_generation != self.generation.load(Ordering::Acquire) {
                return Err(PresentationError::InvalidPane);
            }
            referenced_resources(&state, pane, actor_generation, &packet)?;
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
        if actor_generation != self.generation.load(Ordering::Acquire) {
            return Err(PresentationError::InvalidPane);
        }
        let handle = allocate_resource_handle(
            &mut state,
            pane,
            actor_generation,
            resource.linear_premultiplied_rgba8.len(),
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
                let resource = state
                    .text_layouts
                    .get(layout)
                    .filter(|resource| resource.pane == pane && resource.generation == generation)
                    .ok_or(PresentationError::InvalidResource)?;
                text_layouts.insert(*layout, resource.clone());
            }
            _ => {}
        }
    }
    Ok((images, text_layouts))
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
                .map(|resource| resource.linear_premultiplied_rgba8.len()),
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
    let mut pixels = vec![0_u8; bytes];
    let [red, green, blue, alpha] = options.color;
    let text_color = TextColor::rgba(
        unit_to_u8(red),
        unit_to_u8(green),
        unit_to_u8(blue),
        u8::MAX,
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
                    blend_text_pixel(destination, color, alpha);
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
            linear_premultiplied_rgba8: pixels.into(),
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

fn blend_text_pixel(destination: &mut [u8; 4], color: TextColor, requested_alpha: f32) {
    let [red, green, blue, alpha] = color.as_rgba();
    let opacity = f32::from(alpha) / 255.0 * requested_alpha;
    let inverse = 1.0 - opacity;
    let premultiplied =
        [red, green, blue].map(|channel| srgb_to_linear(f32::from(channel) / 255.0) * opacity);
    let [red, green, blue, output_alpha] = destination;
    for (output, source) in [red, green, blue].into_iter().zip(premultiplied) {
        *output = unit_to_u8((f32::from(*output) / 255.0).mul_add(inverse, source));
    }
    *output_alpha = unit_to_u8((f32::from(*output_alpha) / 255.0).mul_add(inverse, opacity));
}
