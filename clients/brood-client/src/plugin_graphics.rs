use std::{
    collections::{BTreeMap, VecDeque},
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
use brood_wasm::{
    FontIntent, FontStyle, FontWeight, PresentationError, PresentationHost, TextLayoutOptions,
    TextLayoutResult,
};
use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FeatureTag, FontFeatures, FontSystem, Metrics, Shaping,
    Style as CosmicStyle, SwashCache, SwashContent, Weight, Wrap,
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
    text_system: Arc<Mutex<TextSystem>>,
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
    /// Present only when this packet is staged for a synchronized resize.
    pub resize_epoch: Option<u64>,
    pub packet: Packet,
    pub images: BTreeMap<u64, ImageResource>,
    pub text_layouts: BTreeMap<u64, TextLayoutResource>,
    pub glyphs: BTreeMap<GlyphCacheKey, GlyphImageResource>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GraphicsDiagnostics {
    pub accepted_packets: u64,
    pub max_packet_bytes: usize,
    pub max_packet_commands: usize,
    pub text_layouts_created: u64,
    pub text_layouts_dropped: u64,
    pub live_text_layouts: usize,
    pub peak_text_layouts: usize,
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
    resize_epoch: Option<u64>,
    terminal_input_sequence: Option<u64>,
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
    options: TextLayoutOptions,
    plan: TextLayoutPlan,
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
    staged_panes: BTreeMap<(u64, u64), PaneScene>,
    deferred_panes: BTreeMap<u64, PaneScene>,
    active_resize_epochs: BTreeMap<u64, u64>,
    pending_terminal_input_sequences: BTreeMap<u64, VecDeque<u64>>,
    images: BTreeMap<u64, ImageResource>,
    text_layouts: BTreeMap<u64, TextLayoutEntry>,
    accepted_packets: u64,
    max_packet_bytes: usize,
    max_packet_commands: usize,
    text_layouts_created: u64,
    text_layouts_dropped: u64,
    peak_text_layouts: usize,
}

struct ReferencedResources {
    images: BTreeMap<u64, ImageResource>,
    text_layouts: BTreeMap<u64, TextLayoutResource>,
    glyphs: BTreeMap<GlyphCacheKey, GlyphImageResource>,
    ready: bool,
}

fn resize_epoch_is_active(state: &GraphicsState, pane: u64, resize_epoch: u64) -> bool {
    resize_epoch != 0 && state.active_resize_epochs.get(&pane) == Some(&resize_epoch)
}

fn pane_content_is_reusable(
    state: &GraphicsState,
    pane: u64,
    actor_generation: u64,
    resize_epoch: u64,
) -> bool {
    resize_epoch_is_active(state, pane, resize_epoch)
        && state
            .panes
            .get(&pane)
            .is_some_and(|scene| scene.generation == actor_generation)
}

fn take_ready_deferred_scene(state: &mut GraphicsState, pane: u64) -> Option<PaneScene> {
    let ready = state.deferred_panes.get(&pane).is_some_and(|scene| {
        referenced_resources(state, pane, scene.generation, &scene.packet)
            .is_ok_and(|resources| resources.ready)
    });
    ready.then(|| state.deferred_panes.remove(&pane)).flatten()
}

fn promote_ready_deferred_scene(state: &mut GraphicsState, pane: u64) {
    if state.active_resize_epochs.contains_key(&pane) {
        return;
    }
    let Some(pending) = take_ready_deferred_scene(state, pane) else {
        return;
    };
    if state
        .panes
        .get(&pane)
        .is_none_or(|visible| pending.sequence > visible.sequence)
    {
        state.panes.insert(pane, pending);
    }
}

fn abort_resize_epoch_state(
    state: &mut GraphicsState,
    resize_epoch: u64,
) -> Vec<(u64, u64, Option<u64>)> {
    let panes = state
        .active_resize_epochs
        .iter()
        .filter_map(|(pane, epoch)| (*epoch == resize_epoch).then_some(*pane))
        .collect::<Vec<_>>();
    let mut deferred = Vec::new();
    for pane in panes {
        state.active_resize_epochs.remove(&pane);
        state.staged_panes.remove(&(pane, resize_epoch));
        if let Some(scene) = take_ready_deferred_scene(state, pane) {
            deferred.push((pane, scene.sequence, scene.terminal_input_sequence));
            state.panes.insert(pane, scene);
        }
    }
    state
        .staged_panes
        .retain(|(_pane, epoch), _scene| *epoch != resize_epoch);
    deferred
}

fn complete_resize_epoch_state(
    state: &mut GraphicsState,
    resize_epoch: u64,
    promote_tagged: bool,
) -> Vec<(u64, u64, Option<u64>)> {
    let panes = state
        .active_resize_epochs
        .iter()
        .filter_map(|(pane, epoch)| (*epoch == resize_epoch).then_some(*pane))
        .collect::<Vec<_>>();
    let mut deferred = Vec::new();
    for pane in panes {
        state.active_resize_epochs.remove(&pane);
        let tagged = state.staged_panes.remove(&(pane, resize_epoch));
        let ordinary = take_ready_deferred_scene(state, pane);
        match (tagged, ordinary) {
            (Some(mut tagged), Some(ordinary)) if promote_tagged => {
                let terminal_input_sequence = ordinary.terminal_input_sequence;
                if ordinary.sequence > tagged.sequence {
                    deferred.push((pane, ordinary.sequence, terminal_input_sequence));
                    state.panes.insert(pane, ordinary);
                } else {
                    tagged.resize_epoch = None;
                    deferred.push((pane, tagged.sequence, terminal_input_sequence));
                    state.panes.insert(pane, tagged);
                }
            }
            (_tagged, Some(ordinary)) => {
                deferred.push((pane, ordinary.sequence, ordinary.terminal_input_sequence));
                state.panes.insert(pane, ordinary);
            }
            (Some(mut tagged), None) if promote_tagged => {
                tagged.resize_epoch = None;
                state.panes.insert(pane, tagged);
            }
            (_, None) => {}
        }
    }
    state
        .staged_panes
        .retain(|(_pane, epoch), _scene| *epoch != resize_epoch);
    deferred
}

impl PluginGraphicsBridge {
    pub fn new(proxy: EventLoopProxy<ClientEvent>) -> Self {
        let shared = Arc::new(Mutex::new(GraphicsState::default()));
        let text_system = Arc::new(Mutex::new(TextSystem {
            fonts: FontSystem::new(),
            swash: SwashCache::new(),
            glyphs: BTreeMap::new(),
        }));
        let glyph_raster_tx =
            start_glyph_raster_worker(Arc::clone(&shared), Arc::clone(&text_system), proxy.clone());
        Self {
            shared,
            text_system,
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

    pub fn expect_terminal_snapshot(&self, pane: u64, input_sequence: u64) -> Result<()> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        state
            .pending_terminal_input_sequences
            .entry(pane)
            .or_default()
            .push_back(input_sequence);
        drop(state);
        Ok(())
    }

    /// Admit tagged presentation work only for panes in the current resize.
    pub fn activate_resize_epoch(&self, resize_epoch: u64, panes: &[u64]) -> Result<()> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        state.active_resize_epochs.clear();
        state
            .active_resize_epochs
            .extend(panes.iter().copied().map(|pane| (pane, resize_epoch)));
        drop(state);
        Ok(())
    }

    /// Validate a reuse response sent through the ordinary actor mailbox.
    ///
    /// Component imports use the same predicate before publishing their
    /// ready event, so neither path can satisfy a barrier without matching
    /// visible content for the currently admitted pane epoch.
    pub fn validate_reuse_pane_content(
        &self,
        pane: u64,
        actor_generation: u64,
        resize_epoch: u64,
    ) -> Result<(), PresentationError> {
        let state = self
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if !resize_epoch_is_active(&state, pane, resize_epoch) {
            return Err(PresentationError::InvalidArgument);
        }
        if !pane_content_is_reusable(&state, pane, actor_generation, resize_epoch) {
            return Err(PresentationError::InvalidResource);
        }
        Ok(())
    }

    pub fn snapshot(&self, pane: u64) -> Result<Option<GraphicsSnapshot>> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        promote_ready_deferred_scene(&mut state, pane);
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
            resize_epoch: None,
            packet: scene.packet.clone(),
            images: resources.images,
            text_layouts: resources.text_layouts,
            glyphs: resources.glyphs,
        };
        drop(state);
        Ok(Some(snapshot))
    }

    /// Return the content explicitly tagged for one resize epoch without
    /// replacing the pane's currently visible scene.
    pub fn staged_snapshot(
        &self,
        pane: u64,
        resize_epoch: u64,
    ) -> Result<Option<GraphicsSnapshot>> {
        let state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        let Some(scene) = state.staged_panes.get(&(pane, resize_epoch)) else {
            return Ok(None);
        };
        let resources = referenced_resources(&state, pane, scene.generation, &scene.packet)
            .map_err(|_error| anyhow!("staged packet lost a referenced resource"))?;
        if !resources.ready {
            return Ok(None);
        }
        let snapshot = GraphicsSnapshot {
            pane,
            sequence: scene.sequence,
            resize_epoch: Some(resize_epoch),
            packet: scene.packet.clone(),
            images: resources.images,
            text_layouts: resources.text_layouts,
            glyphs: resources.glyphs,
        };
        drop(state);
        Ok(Some(snapshot))
    }

    pub fn discard_staged_epoch(&self, resize_epoch: u64) -> Result<usize> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        let before = state.staged_panes.len();
        state
            .staged_panes
            .retain(|(_pane, epoch), _scene| *epoch != resize_epoch);
        state
            .active_resize_epochs
            .retain(|_pane, epoch| *epoch != resize_epoch);
        Ok(before.saturating_sub(state.staged_panes.len()))
    }

    /// Panes admitted to an epoch, including panes whose tagged GPU scene may
    /// already have been installed for a not-yet-presented frame.
    pub fn active_resize_panes(&self, resize_epoch: u64) -> Result<Vec<u64>> {
        let state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        Ok(state
            .active_resize_epochs
            .iter()
            .filter_map(|(pane, epoch)| (*epoch == resize_epoch).then_some(*pane))
            .collect())
    }

    /// Abort one admitted epoch without making its tagged scenes visible.
    ///
    /// Ordinary scenes submitted while the barrier was active are newer than
    /// the still-visible content. They become the bridge-visible scenes when
    /// the transaction is cancelled, and the renderer recompiles them before
    /// its next presentation.
    pub fn abort_resize_epoch(&self, resize_epoch: u64) -> Result<Vec<(u64, u64, Option<u64>)>> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        let deferred = abort_resize_epoch_state(&mut state, resize_epoch);
        drop(state);
        Ok(deferred)
    }

    /// Close one epoch after its atomic frame has reached `queue.present`.
    /// The newest ordinary draw submitted while each pane was waiting becomes
    /// the bridge's visible scene only at this post-present boundary. Tagged
    /// scenes become bridge-visible only when the renderer actually promoted
    /// them; degraded old-content presentation passes `false`.
    pub fn complete_resize_epoch(
        &self,
        resize_epoch: u64,
        promote_tagged: bool,
    ) -> Result<Vec<(u64, u64, Option<u64>)>> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        let deferred = complete_resize_epoch_state(&mut state, resize_epoch, promote_tagged);
        drop(state);
        Ok(deferred)
    }

    pub fn diagnostics(&self) -> Result<GraphicsDiagnostics> {
        let state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        Ok(GraphicsDiagnostics {
            accepted_packets: state.accepted_packets,
            max_packet_bytes: state.max_packet_bytes,
            max_packet_commands: state.max_packet_commands,
            text_layouts_created: state.text_layouts_created,
            text_layouts_dropped: state.text_layouts_dropped,
            live_text_layouts: state.text_layouts.len(),
            peak_text_layouts: state.peak_text_layouts,
        })
    }
}

fn start_glyph_raster_worker(
    shared: Arc<Mutex<GraphicsState>>,
    text_system: Arc<Mutex<TextSystem>>,
    proxy: EventLoopProxy<ClientEvent>,
) -> Sender<GlyphRasterRequest> {
    let (sender, receiver) = mpsc::channel::<GlyphRasterRequest>();
    let spawn_result = thread::Builder::new()
        .name("brood-glyph-raster".to_owned())
        .spawn(move || {
            while let Ok(request) = receiver.recv() {
                let started = Instant::now();
                let pane = request.pane;
                let handle = request.handle;
                let result = text_system
                    .lock()
                    .map_err(|_poisoned| PresentationError::HostFailure)
                    .map(|mut text_system| {
                        rasterize_text_layout(&mut text_system, &request.plan, request.options)
                    });
                let elapsed = started.elapsed();
                finish_glyph_raster(&shared, &proxy, &request, result);
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
    request: &GlyphRasterRequest,
    result: Result<TextLayoutResource, PresentationError>,
) {
    let notifications = {
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
        state
            .panes
            .get(&request.pane)
            .into_iter()
            .chain(state.deferred_panes.get(&request.pane))
            .chain(
                state
                    .staged_panes
                    .range((request.pane, 0)..=(request.pane, u64::MAX))
                    .map(|(_key, scene)| scene),
            )
            .filter(|scene| {
                scene.generation == request.generation
                    && packet_references_text_layout(&scene.packet, request.handle)
            })
            .filter_map(|scene| {
                referenced_resources(&state, request.pane, request.generation, &scene.packet)
                    .is_ok_and(|resources| resources.ready)
                    .then_some((
                        scene.sequence,
                        scene.resize_epoch,
                        scene.terminal_input_sequence,
                    ))
            })
            .collect::<Vec<_>>()
    };
    for (sequence, resize_epoch, terminal_input_sequence) in notifications {
        if proxy
            .send_event(ClientEvent::PluginGraphicsChanged {
                pane: request.pane,
                sequence,
                resize_epoch,
                terminal_input_sequence,
                ready_at: Instant::now(),
            })
            .is_err()
        {
            eprintln!(
                "failed to publish ready plugin text scene pane={} sequence={sequence} resize_epoch={resize_epoch:?}",
                request.pane
            );
        }
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
            let previous_text_layouts = state.text_layouts.len();
            state
                .text_layouts
                .retain(|_, resource| resource.pane != self.pane);
            state.pending_terminal_input_sequences.remove(&self.pane);
            state
                .staged_panes
                .retain(|(pane, _epoch), _scene| *pane != self.pane);
            state.deferred_panes.remove(&self.pane);
            state.active_resize_epochs.remove(&self.pane);
            state.text_layouts_dropped = state.text_layouts_dropped.saturating_add(
                u64::try_from(previous_text_layouts.saturating_sub(state.text_layouts.len()))
                    .unwrap_or(u64::MAX),
            );
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

impl Drop for ActorPresentationHost {
    fn drop(&mut self) {
        if let Ok(mut state) = self.bridge.shared.lock() {
            state
                .images
                .retain(|_, resource| resource.pane != self.pane);
            let previous_text_layouts = state.text_layouts.len();
            state
                .text_layouts
                .retain(|_, resource| resource.pane != self.pane);
            state.pending_terminal_input_sequences.remove(&self.pane);
            state
                .staged_panes
                .retain(|(pane, _epoch), _scene| *pane != self.pane);
            state.deferred_panes.remove(&self.pane);
            state.active_resize_epochs.remove(&self.pane);
            let removed = previous_text_layouts.saturating_sub(state.text_layouts.len());
            state.text_layouts_dropped = state
                .text_layouts_dropped
                .saturating_add(u64::try_from(removed).unwrap_or(u64::MAX));
            state.panes.remove(&self.pane);
        }
    }
}

impl ActorPresentationHost {
    fn submit_draw_packet_inner(
        &self,
        actor_generation: u64,
        pane: u64,
        resize_epoch: Option<u64>,
        packet: &[u8],
    ) -> Result<u64, PresentationError> {
        self.authorize(actor_generation, pane)?;
        if resize_epoch == Some(0) {
            return Err(PresentationError::InvalidArgument);
        }
        let packet_bytes = packet.len();
        let packet = Packet::decode(packet).map_err(|error| match error {
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
            if let Some(resize_epoch) = resize_epoch
                && !resize_epoch_is_active(&state, pane, resize_epoch)
            {
                return Err(PresentationError::InvalidArgument);
            }
            referenced_resources(&state, pane, actor_generation, &packet)?
        };
        let candidate = GraphicsSnapshot {
            pane,
            sequence: 0,
            resize_epoch,
            packet,
            images: resources.images,
            text_layouts: resources.text_layouts,
            glyphs: resources.glyphs,
        };
        crate::plugin_gpu::validate_snapshot_lowering(&candidate)?;
        let packet = candidate.packet;
        let (sequence, terminal_input_sequence, previous, ready, deferred_ordinary) = {
            let mut state = self
                .bridge
                .shared
                .lock()
                .map_err(|_poisoned| PresentationError::HostFailure)?;
            if actor_generation != self.generation.load(Ordering::Acquire) {
                return Err(PresentationError::InvalidPane);
            }
            if let Some(resize_epoch) = resize_epoch
                && !resize_epoch_is_active(&state, pane, resize_epoch)
            {
                return Err(PresentationError::InvalidArgument);
            }
            let resources = referenced_resources(&state, pane, actor_generation, &packet)?;
            state.accepted_packets = state.accepted_packets.saturating_add(1);
            state.max_packet_bytes = state.max_packet_bytes.max(packet_bytes);
            state.max_packet_commands = state.max_packet_commands.max(packet.commands.len());
            state.next_sequence = state
                .next_sequence
                .checked_add(1)
                .ok_or(PresentationError::LimitReached)?;
            let sequence = state.next_sequence;
            let terminal_input_sequence = resize_epoch
                .is_none()
                .then(|| {
                    state
                        .pending_terminal_input_sequences
                        .get_mut(&pane)
                        .and_then(VecDeque::pop_front)
                })
                .flatten();
            let scene = PaneScene {
                generation: actor_generation,
                sequence,
                resize_epoch,
                terminal_input_sequence,
                packet,
            };
            let deferred_ordinary = resize_epoch.is_none()
                && (state.active_resize_epochs.contains_key(&pane) || !resources.ready);
            let previous = if let Some(resize_epoch) = resize_epoch {
                state.staged_panes.insert((pane, resize_epoch), scene)
            } else if deferred_ordinary {
                state.deferred_panes.insert(pane, scene)
            } else {
                state.panes.insert(pane, scene)
            };
            drop(state);
            (
                sequence,
                terminal_input_sequence,
                previous,
                resources.ready,
                deferred_ordinary,
            )
        };
        if ready
            && self
                .bridge
                .proxy
                .send_event(ClientEvent::PluginGraphicsChanged {
                    pane,
                    sequence,
                    resize_epoch,
                    terminal_input_sequence,
                    ready_at: Instant::now(),
                })
                .is_err()
        {
            if let Ok(mut state) = self.bridge.shared.lock() {
                match previous {
                    Some(previous) => {
                        if let Some(resize_epoch) = resize_epoch {
                            state.staged_panes.insert((pane, resize_epoch), previous);
                        } else if deferred_ordinary {
                            state.deferred_panes.insert(pane, previous);
                        } else {
                            state.panes.insert(pane, previous);
                        }
                    }
                    None => {
                        if let Some(resize_epoch) = resize_epoch {
                            state.staged_panes.remove(&(pane, resize_epoch));
                        } else if deferred_ordinary {
                            state.deferred_panes.remove(&pane);
                        } else {
                            state.panes.remove(&pane);
                        }
                    }
                }
            }
            return Err(PresentationError::HostFailure);
        }
        Ok(sequence)
    }
}

impl PresentationHost for ActorPresentationHost {
    fn submit_draw_packet(
        &self,
        actor_generation: u64,
        pane: u64,
        packet: Vec<u8>,
    ) -> Result<u64, PresentationError> {
        self.submit_draw_packet_inner(actor_generation, pane, None, &packet)
    }

    fn submit_tagged_draw_packet(
        &self,
        actor_generation: u64,
        pane: u64,
        resize_epoch: u64,
        packet: Vec<u8>,
    ) -> Result<u64, PresentationError> {
        self.submit_draw_packet_inner(actor_generation, pane, Some(resize_epoch), &packet)
    }

    fn reuse_pane_content(
        &self,
        actor_generation: u64,
        pane: u64,
        resize_epoch: u64,
    ) -> Result<(), PresentationError> {
        self.authorize(actor_generation, pane)?;
        self.bridge
            .validate_reuse_pane_content(pane, actor_generation, resize_epoch)?;
        self.bridge
            .proxy
            .send_event(ClientEvent::PluginContentReused {
                pane,
                resize_epoch,
                ready_at: Instant::now(),
            })
            .map_err(|_closed| PresentationError::HostFailure)
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
            let mut text_system = self
                .bridge
                .text_system
                .lock()
                .map_err(|_poisoned| PresentationError::HostFailure)?;
            create_text_layout_plan(&mut text_system.fonts, &text, options)?
        };
        let metrics = plan.metrics;
        let byte_len = plan.byte_len;
        let mut state = self
            .bridge
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if actor_generation != self.generation.load(Ordering::Acquire) {
            return Err(PresentationError::InvalidPane);
        }
        let handle = allocate_resource_handle(&mut state, pane, actor_generation, byte_len)?;
        state.text_layouts.insert(
            handle,
            TextLayoutEntry {
                pane,
                generation: actor_generation,
                width: plan.width,
                height: plan.height,
                byte_len,
                state: TextLayoutState::Pending,
            },
        );
        state.text_layouts_created = state.text_layouts_created.saturating_add(1);
        state.peak_text_layouts = state.peak_text_layouts.max(state.text_layouts.len());
        drop(state);
        let request = GlyphRasterRequest {
            handle,
            pane,
            generation: actor_generation,
            options,
            plan,
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
                byte_len,
                elapsed.as_micros(),
            );
        }
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
        if state
            .panes
            .values()
            .chain(state.deferred_panes.values())
            .chain(state.staged_panes.values())
            .any(|scene| {
                scene.packet.commands.iter().any(|command| match command {
                    Command::DrawImage { image, .. } => *image == resource,
                    Command::DrawTextLayout { layout, .. } => *layout == resource,
                    _ => false,
                })
            })
        {
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
            state.text_layouts_dropped = state.text_layouts_dropped.saturating_add(1);
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

#[derive(Debug)]
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
    let mut features = FontFeatures::new();
    if options.font_intent == FontIntent::Monospace {
        features
            .disable(FeatureTag::KERNING)
            .disable(FeatureTag::STANDARD_LIGATURES)
            .disable(FeatureTag::CONTEXTUAL_LIGATURES)
            .disable(FeatureTag::CONTEXTUAL_ALTERNATES)
            .disable(FeatureTag::DISCRETIONARY_LIGATURES);
        buffer.set_wrap(Wrap::None);
    }
    let attrs = Attrs::new()
        .family(match options.font_intent {
            FontIntent::Ui => Family::SansSerif,
            FontIntent::Monospace => Family::Monospace,
        })
        .weight(match options.font_weight {
            FontWeight::Normal => Weight::NORMAL,
            FontWeight::Bold => Weight::BOLD,
        })
        .style(match options.font_style {
            FontStyle::Normal => CosmicStyle::Normal,
            FontStyle::Italic => CosmicStyle::Italic,
        })
        .font_features(features);
    let shaping = if options.font_intent == FontIntent::Monospace && text.is_ascii() {
        Shaping::Basic
    } else {
        Shaping::Advanced
    };
    buffer.set_text(text, &attrs, shaping, None);
    buffer.shape_until_scroll(fonts, false);

    let mut measured_width = 0.0_f32;
    let mut measured_height = 0.0_f32;
    let mut first_baseline = 0.0_f32;
    let mut line_count = 0_u32;
    let mut cell_width = 0.0_f32;
    for run in buffer.layout_runs() {
        if line_count == 0 {
            first_baseline = run.line_y;
        }
        measured_width = measured_width.max(run.line_w);
        measured_height = measured_height.max(run.line_top + run.line_height);
        line_count = line_count.saturating_add(1);
        if options.font_intent == FontIntent::Monospace && cell_width == 0.0 {
            cell_width = run.glyphs.first().map_or(0.0, |glyph| glyph.w);
        }
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
            cell_width,
            line_height: options.line_height,
        },
        width,
        height,
        byte_len: bytes,
    })
}

fn rasterize_text_layout(
    text_system: &mut TextSystem,
    plan: &TextLayoutPlan,
    options: TextLayoutOptions,
) -> TextLayoutResource {
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
    TextLayoutResource {
        width: plan.width,
        height: plan.height,
        glyphs: sprites.into(),
        glyph_images,
    }
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
                let coverage = source.iter().take(3).copied().max().unwrap_or(0);
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
    fn pending_text_dependencies_hold_a_scene_until_all_are_ready() -> Result<()> {
        let mut state = GraphicsState::default();
        state.text_layouts.insert(1, pending_text_entry());
        state.text_layouts.insert(2, pending_text_entry());
        let packet = text_packet(&[1, 2]);

        let pending = referenced_resources(&state, 7, 11, &packet)?;
        anyhow::ensure!(!pending.ready, "pending dependencies reported ready");

        state
            .text_layouts
            .get_mut(&1)
            .ok_or_else(|| anyhow!("first pending text layout is missing"))?
            .state = TextLayoutState::Ready(ready_text_layout());
        let partly_ready = referenced_resources(&state, 7, 11, &packet)?;
        anyhow::ensure!(
            !partly_ready.ready,
            "partly ready dependencies reported ready"
        );

        state
            .text_layouts
            .get_mut(&2)
            .ok_or_else(|| anyhow!("second pending text layout is missing"))?
            .state = TextLayoutState::Ready(ready_text_layout());
        let ready = referenced_resources(&state, 7, 11, &packet)?;
        anyhow::ensure!(ready.ready, "ready dependencies remained pending");
        anyhow::ensure!(
            ready.text_layouts.len() == 2,
            "ready dependency count changed"
        );
        anyhow::ensure!(ready.glyphs.is_empty(), "unexpected glyph dependencies");
        Ok(())
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

    #[test]
    fn tagged_work_is_admitted_only_for_the_current_pane_epoch() {
        let mut state = GraphicsState::default();
        state.active_resize_epochs.insert(7, 41);

        assert!(resize_epoch_is_active(&state, 7, 41));
        assert!(!resize_epoch_is_active(&state, 7, 40));
        assert!(!resize_epoch_is_active(&state, 8, 41));
        assert!(!resize_epoch_is_active(&state, 7, 0));

        assert!(!pane_content_is_reusable(&state, 7, 2, 41));
        state.panes.insert(
            7,
            PaneScene {
                generation: 2,
                sequence: 1,
                resize_epoch: None,
                terminal_input_sequence: None,
                packet: Packet::default(),
            },
        );
        assert!(pane_content_is_reusable(&state, 7, 2, 41));
        assert!(!pane_content_is_reusable(&state, 7, 3, 41));
        assert!(!pane_content_is_reusable(&state, 7, 2, 42));
    }

    #[test]
    fn abort_discards_tagged_content_and_releases_newest_ordinary_scene() {
        let scene = |sequence, resize_epoch, terminal_input_sequence| PaneScene {
            generation: 2,
            sequence,
            resize_epoch,
            terminal_input_sequence,
            packet: Packet::default(),
        };
        let mut state = GraphicsState::default();
        state.panes.insert(7, scene(1, None, None));
        state.panes.insert(8, scene(4, None, None));
        state.staged_panes.insert((7, 41), scene(2, Some(41), None));
        state.staged_panes.insert((8, 41), scene(5, Some(41), None));
        state.deferred_panes.insert(7, scene(3, None, Some(19)));
        state.active_resize_epochs.insert(7, 41);
        state.active_resize_epochs.insert(8, 41);

        let deferred = abort_resize_epoch_state(&mut state, 41);

        assert_eq!(deferred, vec![(7, 3, Some(19))]);
        assert_eq!(state.panes.get(&7).map(|scene| scene.sequence), Some(3));
        assert_eq!(state.panes.get(&8).map(|scene| scene.sequence), Some(4));
        assert!(state.staged_panes.is_empty());
        assert!(state.deferred_panes.is_empty());
        assert!(state.active_resize_epochs.is_empty());
        state.active_resize_epochs.insert(7, 42);
        assert!(pane_content_is_reusable(&state, 7, 2, 42));
    }

    #[test]
    fn supersession_releases_deferred_panes_dropped_from_the_next_wait_set() {
        let scene = |sequence, resize_epoch, terminal_input_sequence| PaneScene {
            generation: 2,
            sequence,
            resize_epoch,
            terminal_input_sequence,
            packet: Packet::default(),
        };
        let mut state = GraphicsState::default();
        state.panes.insert(7, scene(1, None, None));
        state.panes.insert(8, scene(4, None, None));
        state.staged_panes.insert((7, 41), scene(2, Some(41), None));
        state.staged_panes.insert((8, 41), scene(5, Some(41), None));
        state.deferred_panes.insert(8, scene(6, None, Some(20)));
        state.active_resize_epochs.insert(7, 41);
        state.active_resize_epochs.insert(8, 41);

        assert_eq!(
            abort_resize_epoch_state(&mut state, 41),
            vec![(8, 6, Some(20))]
        );
        assert_eq!(state.panes.get(&8).map(|scene| scene.sequence), Some(6));

        // The superseding epoch waits only for A. B remains ordinary and
        // visible instead of being hidden until an unrelated later resize.
        state.active_resize_epochs.insert(7, 42);
        assert!(!resize_epoch_is_active(&state, 8, 42));
        assert_eq!(state.panes.get(&8).map(|scene| scene.sequence), Some(6));

        state.active_resize_epochs.clear();
        state.active_resize_epochs.insert(8, 43);
        assert!(pane_content_is_reusable(&state, 8, 2, 43));
    }

    #[test]
    fn promoted_before_present_abort_retains_only_the_old_bridge_scene() {
        let scene = |sequence, resize_epoch| PaneScene {
            generation: 2,
            sequence,
            resize_epoch,
            terminal_input_sequence: None,
            packet: Packet::default(),
        };
        let mut state = GraphicsState::default();
        state.panes.insert(7, scene(1, None));
        state.staged_panes.insert((7, 41), scene(2, Some(41)));
        state.active_resize_epochs.insert(7, 41);

        // The renderer may have installed sequence 2 into its GPU map before
        // a skipped presentation, but the bridge still identifies sequence 1
        // as the only scene with visible evidence.
        assert!(abort_resize_epoch_state(&mut state, 41).is_empty());
        assert_eq!(state.panes.get(&7).map(|scene| scene.sequence), Some(1));
        assert!(state.staged_panes.is_empty());
        assert!(state.active_resize_epochs.is_empty());
    }

    #[test]
    fn abort_keeps_old_scene_visible_until_deferred_resources_are_ready() -> Result<()> {
        let mut state = GraphicsState::default();
        state.panes.insert(
            7,
            PaneScene {
                generation: 11,
                sequence: 1,
                resize_epoch: None,
                terminal_input_sequence: None,
                packet: Packet::default(),
            },
        );
        state.text_layouts.insert(1, pending_text_entry());
        state.deferred_panes.insert(
            7,
            PaneScene {
                generation: 11,
                sequence: 3,
                resize_epoch: None,
                terminal_input_sequence: Some(20),
                packet: text_packet(&[1]),
            },
        );
        state.active_resize_epochs.insert(7, 41);

        anyhow::ensure!(abort_resize_epoch_state(&mut state, 41).is_empty());
        anyhow::ensure!(state.panes.get(&7).map(|scene| scene.sequence) == Some(1));
        anyhow::ensure!(state.deferred_panes.get(&7).map(|scene| scene.sequence) == Some(3));
        promote_ready_deferred_scene(&mut state, 7);
        anyhow::ensure!(state.panes.get(&7).map(|scene| scene.sequence) == Some(1));

        state
            .text_layouts
            .get_mut(&1)
            .ok_or_else(|| anyhow!("pending layout disappeared"))?
            .state = TextLayoutState::Ready(ready_text_layout());
        promote_ready_deferred_scene(&mut state, 7);
        anyhow::ensure!(state.panes.get(&7).map(|scene| scene.sequence) == Some(3));
        anyhow::ensure!(state.deferred_panes.is_empty());
        Ok(())
    }

    #[test]
    fn completion_keeps_tagged_scene_until_newer_deferred_resources_are_ready() -> Result<()> {
        let mut state = GraphicsState::default();
        state.panes.insert(
            7,
            PaneScene {
                generation: 11,
                sequence: 1,
                resize_epoch: None,
                terminal_input_sequence: None,
                packet: Packet::default(),
            },
        );
        state.staged_panes.insert(
            (7, 41),
            PaneScene {
                generation: 11,
                sequence: 2,
                resize_epoch: Some(41),
                terminal_input_sequence: None,
                packet: Packet::default(),
            },
        );
        state.text_layouts.insert(1, pending_text_entry());
        state.deferred_panes.insert(
            7,
            PaneScene {
                generation: 11,
                sequence: 3,
                resize_epoch: None,
                terminal_input_sequence: Some(20),
                packet: text_packet(&[1]),
            },
        );
        state.active_resize_epochs.insert(7, 41);

        anyhow::ensure!(complete_resize_epoch_state(&mut state, 41, true).is_empty());
        anyhow::ensure!(state.panes.get(&7).map(|scene| scene.sequence) == Some(2));
        anyhow::ensure!(state.deferred_panes.get(&7).map(|scene| scene.sequence) == Some(3));

        state
            .text_layouts
            .get_mut(&1)
            .ok_or_else(|| anyhow!("pending layout disappeared"))?
            .state = TextLayoutState::Ready(ready_text_layout());
        promote_ready_deferred_scene(&mut state, 7);
        anyhow::ensure!(state.panes.get(&7).map(|scene| scene.sequence) == Some(3));
        anyhow::ensure!(state.deferred_panes.is_empty());
        Ok(())
    }

    #[test]
    fn degraded_completion_never_exposes_unpresented_tagged_content() {
        let scene = |sequence, resize_epoch, terminal_input_sequence| PaneScene {
            generation: 2,
            sequence,
            resize_epoch,
            terminal_input_sequence,
            packet: Packet::default(),
        };

        let mut degraded = GraphicsState::default();
        degraded.panes.insert(7, scene(1, None, None));
        degraded
            .staged_panes
            .insert((7, 41), scene(2, Some(41), None));
        degraded.active_resize_epochs.insert(7, 41);
        assert!(complete_resize_epoch_state(&mut degraded, 41, false).is_empty());
        assert_eq!(degraded.panes.get(&7).map(|scene| scene.sequence), Some(1));
        degraded.active_resize_epochs.insert(7, 42);
        assert!(pane_content_is_reusable(&degraded, 7, 2, 42));

        let mut promoted = GraphicsState::default();
        promoted.panes.insert(7, scene(1, None, None));
        promoted
            .staged_panes
            .insert((7, 41), scene(2, Some(41), None));
        promoted.active_resize_epochs.insert(7, 41);
        assert!(complete_resize_epoch_state(&mut promoted, 41, true).is_empty());
        assert_eq!(promoted.panes.get(&7).map(|scene| scene.sequence), Some(2));
        assert_eq!(
            promoted.panes.get(&7).and_then(|scene| scene.resize_epoch),
            None
        );

        let mut newer_tagged = GraphicsState::default();
        newer_tagged.panes.insert(7, scene(1, None, None));
        newer_tagged
            .deferred_panes
            .insert(7, scene(2, None, Some(18)));
        newer_tagged
            .staged_panes
            .insert((7, 41), scene(3, Some(41), None));
        newer_tagged.active_resize_epochs.insert(7, 41);
        assert_eq!(
            complete_resize_epoch_state(&mut newer_tagged, 41, true),
            vec![(7, 3, Some(18))]
        );
        assert_eq!(
            newer_tagged.panes.get(&7).map(|scene| scene.sequence),
            Some(3)
        );

        let mut newer_ordinary = GraphicsState::default();
        newer_ordinary.panes.insert(7, scene(1, None, None));
        newer_ordinary
            .staged_panes
            .insert((7, 41), scene(2, Some(41), None));
        newer_ordinary
            .deferred_panes
            .insert(7, scene(3, None, Some(19)));
        newer_ordinary.active_resize_epochs.insert(7, 41);
        assert_eq!(
            complete_resize_epoch_state(&mut newer_ordinary, 41, true),
            vec![(7, 3, Some(19))]
        );
        assert_eq!(
            newer_ordinary.panes.get(&7).map(|scene| scene.sequence),
            Some(3)
        );

        let mut deferred = GraphicsState::default();
        deferred.panes.insert(7, scene(1, None, None));
        deferred
            .staged_panes
            .insert((7, 41), scene(2, Some(41), None));
        deferred.deferred_panes.insert(7, scene(3, None, Some(19)));
        deferred.active_resize_epochs.insert(7, 41);
        assert_eq!(
            complete_resize_epoch_state(&mut deferred, 41, false),
            vec![(7, 3, Some(19))]
        );
        assert_eq!(deferred.panes.get(&7).map(|scene| scene.sequence), Some(3));
    }
}
