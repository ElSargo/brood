use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    ops::Deref,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::Instant,
};

use anyhow::{Context, Result, anyhow};
use brood_graphics::{Command, Packet, PacketError};
use brood_wasm::presentation::{
    ActorFrameDraw, RenderHint as GuestRenderHint, RenderToken as GuestRenderToken,
};
use brood_wasm::{
    FontIntent, FontStyle, FontWeight, PresentationError, PresentationHost, TextLayoutOptions,
    TextLayoutResult,
    facet_resources::FacetResourceBackend,
    snapshot_budget::{BudgetedValue, ByteBudget},
};
use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FeatureTag, FontFeatures, FontSystem, Metrics, Shaping,
    Style as CosmicStyle, SwashCache, SwashContent, Weight, Wrap,
};
use serde::Serialize;
use winit::event_loop::EventLoopProxy;

use crate::{
    ClientEvent,
    plugin_facets::PreparedFacetRender,
    render_scheduler::{
        ActorSchedulerProgress, ActorScope, COLD_HOST_COST_NS, DIAGNOSTIC_WARMUP_OPPORTUNITIES,
        FramePredictor, LateFinishDiagnostic, MAX_RELEVANT_PANES, MAX_RENDER_ACTORS,
        RenderScheduler, RenderToken, SchedulerMetrics,
    },
};

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESOURCE_DIMENSION: u32 = 4_096;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_TEXT_LAYOUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESOURCES_PER_PANE: usize = 1_024;
const MAX_RESOURCE_BYTES_PER_PANE: usize = 64 * 1024 * 1024;
const GLYPH_RASTER_QUEUE_CAPACITY: usize = 128;
const MAX_GLYPH_CACHE_ENTRIES: usize = 4_096;
const MAX_GLYPH_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TEXT_BACKING_REUSE_SCAN: usize = 64;
const COLD_WINIT_REDRAW_HOP_NS: u64 = 1_000_000;
const MAX_WINIT_REDRAW_HOP_NS: u64 = 50_000_000;
const DEFAULT_RASTER_SCALE: f32 = 1.0;
const SHARED_PACKET_METADATA_BYTES: usize = std::mem::size_of::<Packet>()
    + std::mem::size_of::<BudgetedValue<()>>()
    + 4 * std::mem::size_of::<usize>();

/// Immutable packet storage shared by CPU snapshots and prepared native scenes.
///
/// Facet packets retain their generation budget reservation for every clone.
/// The inner `Arc` is deliberately not exposed, so packet ownership cannot be
/// detached from that reservation.
#[derive(Clone, Debug)]
pub struct SharedPacket {
    packet: Arc<Packet>,
    _reservation: Option<Arc<BudgetedValue<()>>>,
}

impl SharedPacket {
    pub(crate) fn decode_facet(
        bytes: &[u8],
        budget: &ByteBudget,
        actor_generation: u64,
    ) -> Result<Self, PresentationError> {
        if budget.generation() != actor_generation {
            return Err(PresentationError::ActorStopped);
        }
        let retained_bytes = Packet::decode_allocation_bound(bytes)
            .map_err(packet_error_to_presentation)?
            .checked_add(SHARED_PACKET_METADATA_BYTES)
            .ok_or(PresentationError::LimitReached)?;
        let reservation = BudgetedValue::capture(budget, retained_bytes, || {
            Ok::<(), std::convert::Infallible>(())
        })
        .map_err(|_error| PresentationError::LimitReached)?;
        let packet = Packet::decode(bytes).map_err(packet_error_to_presentation)?;
        if budget.generation() != actor_generation || !budget.is_active() {
            return Err(PresentationError::ActorStopped);
        }
        Ok(Self {
            packet: Arc::new(packet),
            _reservation: Some(Arc::new(reservation)),
        })
    }
}

impl From<Packet> for SharedPacket {
    fn from(packet: Packet) -> Self {
        Self {
            packet: Arc::new(packet),
            _reservation: None,
        }
    }
}

impl Deref for SharedPacket {
    type Target = Packet;

    fn deref(&self) -> &Self::Target {
        &self.packet
    }
}

const fn packet_error_to_presentation(error: PacketError) -> PresentationError {
    match error {
        PacketError::Limit(_) => PresentationError::LimitReached,
        PacketError::Invalid(_) => PresentationError::InvalidPacket,
    }
}

#[derive(Clone)]
pub struct PluginGraphicsBridge {
    shared: Arc<Mutex<GraphicsState>>,
    text_system: Arc<Mutex<TextSystem>>,
    glyph_raster_tx: SyncSender<GlyphRasterWork>,
    proxy: EventLoopProxy<ClientEvent>,
    render_scheduler: Arc<Mutex<RenderScheduler>>,
    render_clock_origin: Instant,
    host_tail_ns: Arc<AtomicU64>,
    winit_redraw_hop_ns: Arc<AtomicU64>,
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
    actor: String,
    panes: BTreeSet<u64>,
    generation: AtomicU64,
    retired_generation: AtomicU64,
    bridge: PluginGraphicsBridge,
}

/// Pure-facet resource adapter retaining the actor generation it was issued for.
#[derive(Debug)]
pub struct FacetPresentationBackend {
    presentation: Arc<ActorPresentationHost>,
    actor_generation: u64,
}

#[derive(Clone, Debug)]
pub struct GraphicsSnapshot {
    /// The complete texture extent validated by the paired facet contract.
    /// Transparent and rounded canvases are complete without an opaque fill.
    pub facet_extent: Option<(u32, u32)>,
    pub pane: u64,
    pub sequence: u64,
    /// Present only when this packet is staged for a synchronized resize.
    pub resize_epoch: Option<u64>,
    pub packet: SharedPacket,
    pub images: BTreeMap<u64, ImageResource>,
    pub text_layouts: BTreeMap<u64, TextLayoutResource>,
    pub glyphs: BTreeMap<GlyphCacheKey, GlyphImageResource>,
    pub(crate) facet_frame_lease: Option<Arc<brood_wasm::facet_frames::PaneFrame>>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GraphicsDiagnostics {
    pub accepted_packets: u64,
    pub max_packet_bytes: usize,
    pub max_packet_commands: usize,
    pub text_layouts_created: u64,
    pub text_layouts_dropped: u64,
    pub live_text_layouts: usize,
    pub peak_text_layouts: usize,
    pub glyph_raster_backpressure: u64,
    pub render_scheduler: SchedulerMetrics,
    pub key_to_present_p50_ns: u64,
    pub key_to_present_p95_ns: u64,
    pub key_to_present_p99_ns: u64,
    pub key_to_present_max_ns: u64,
    pub key_to_present_stddev_ns: u64,
    pub frame_interval_samples: usize,
    pub frame_interval_p95_ns: u64,
    pub frame_interval_p99_ns: u64,
    pub key_to_present_ns: Vec<u64>,
    pub frame_intervals_ns: Vec<u64>,
    pub measured_key_to_present_ns: Vec<u64>,
    pub measured_frame_intervals_ns: Vec<u64>,
    pub warmup_opportunities: u64,
    pub measured_opportunities: u64,
    pub measured_deadline_misses: u64,
    pub measured_target_misses: u64,
    pub late_finish_diagnostics: Vec<LateFinishDiagnostic>,
    pub late_finish_diagnostics_dropped: u64,
    pub timing_observations: Vec<[u64; 7]>,
    pub actor_progress: Vec<ActorSchedulerProgress>,
    pub actor_frames_accepted: u64,
    pub actor_frames_ready: u64,
    pub actor_frames_promoted: u64,
    pub actor_frames_promoted_multi_pane: u64,
    pub actor_frame_partial_promotion_violations: u64,
    pub visibility_changes: u64,
    pub max_visible_panes: usize,
    pub global_host_tail_estimate_ns: u64,
    pub winit_redraw_hop_estimate_ns: u64,
}

pub struct RenderSchedulerService {
    pub hints: Vec<(ActorScope, GuestRenderHint)>,
    pub composition_tokens: Vec<u64>,
    pub request_redraw_target_ns: Option<u64>,
}

#[derive(Default)]
pub struct CompositionRelease {
    pub tokens: Vec<u64>,
    pub request_redraw_target_ns: Option<u64>,
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
    packet: SharedPacket,
    facet_frame: Option<Arc<brood_wasm::facet_frames::PaneFrame>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActorFrameGraphicsChange {
    pub pane: u64,
    pub sequence: u64,
    pub resize_epoch: Option<u64>,
    pub terminal_input_sequence: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActorFramePromotion {
    Claimed,
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActorFramePreparationClaim {
    Prepare,
    PrepareDeferred,
    Stale,
}

#[derive(Debug, Default)]
pub struct DeferredActorFrameResolutionPlan {
    pub retain: BTreeSet<u64>,
    pub commit: BTreeMap<u64, BTreeMap<u64, u64>>,
    cancelled_tokens: Vec<u64>,
}

struct TextSystem {
    fonts: FontSystem,
    swash: SwashCache,
    glyphs: BTreeMap<GlyphCacheKey, Option<GlyphImageResource>>,
    glyph_cache_bytes: usize,
}

#[derive(Debug)]
struct GlyphRasterRequest {
    handle: u64,
    pane: u64,
    generation: u64,
    options: TextLayoutOptions,
    plan: TextLayoutPlan,
    raster_scale: f32,
}

#[derive(Debug)]
enum GlyphRasterWork {
    Layout(Box<GlyphRasterRequest>),
    ScaleChanged(f32),
}

#[derive(Clone, Debug)]
pub struct TextLayoutResource {
    pub width: u32,
    pub height: u32,
    pub glyphs: Arc<[GlyphSprite]>,
    pub glyph_images: BTreeMap<GlyphCacheKey, GlyphImageResource>,
    pub raster_scale: f32,
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
    pub x: f32,
    pub y: f32,
}

#[derive(Debug)]
struct TextLayoutEntry {
    pane: u64,
    generation: u64,
    width: u32,
    height: u32,
    byte_len: usize,
    includes_source_text: bool,
    text: Arc<str>,
    options: TextLayoutOptions,
    metrics: TextLayoutResult,
    font_context_revision: u64,
    raster_scale: f32,
    state: TextLayoutState,
}

#[derive(Debug)]
enum TextLayoutState {
    Pending,
    Ready(TextLayoutResource),
    Failed,
}

struct ReusableTextLayout {
    retained_bytes: usize,
    metrics: TextLayoutResult,
    text: Arc<str>,
    resource: TextLayoutResource,
}

fn reusable_text_layout(
    state: &GraphicsState,
    actor_generation: u64,
    text: &str,
    options: TextLayoutOptions,
    raster_scale: f32,
    includes_source_text: bool,
) -> Option<ReusableTextLayout> {
    // Handles increase monotonically, so reverse iteration considers the
    // newest live entries first. Bound lookup independently of the registry's
    // 16K resource ceiling; an older match safely falls back to shaping.
    state
        .text_layouts
        .values()
        .rev()
        .take(MAX_TEXT_BACKING_REUSE_SCAN)
        .find_map(|entry| {
            if entry.generation != actor_generation
                || entry.includes_source_text != includes_source_text
                || entry.text.as_ref() != text
                || entry.options != options
                || entry.font_context_revision != state.font_context_revision
                || entry.raster_scale.to_bits() != raster_scale.to_bits()
            {
                return None;
            }
            let TextLayoutState::Ready(resource) = &entry.state else {
                return None;
            };
            Some(ReusableTextLayout {
                retained_bytes: entry.byte_len,
                metrics: entry.metrics,
                text: Arc::clone(&entry.text),
                resource: resource.clone(),
            })
        })
}

#[derive(Debug, Default)]
struct GraphicsState {
    next_sequence: u64,
    next_resource: u64,
    panes: BTreeMap<u64, PaneScene>,
    staged_panes: BTreeMap<(u64, u64), PaneScene>,
    deferred_panes: BTreeMap<u64, PaneScene>,
    active_resize_epochs: BTreeMap<u64, u64>,
    latest_complete_state_watermarks: BTreeMap<u64, u64>,
    images: BTreeMap<u64, ImageResource>,
    text_layouts: BTreeMap<u64, TextLayoutEntry>,
    accepted_packets: u64,
    max_packet_bytes: usize,
    max_packet_commands: usize,
    text_layouts_created: u64,
    text_layouts_dropped: u64,
    peak_text_layouts: usize,
    glyph_raster_backpressure: u64,
    visible_panes: BTreeSet<u64>,
    pending_actor_frames: BTreeMap<u64, PendingGraphicsActorFrame>,
    deferred_actor_frames: BTreeMap<u64, PendingGraphicsActorFrame>,
    settled_actor_frame_correlations: BTreeMap<u64, Vec<(u64, u64, Option<u64>)>>,
    actor_panes: BTreeMap<ActorScope, BTreeSet<u64>>,
    next_facet_targets: BTreeMap<ActorScope, u64>,
    actor_frames_accepted: u64,
    actor_frames_ready: u64,
    actor_frames_promoted: u64,
    actor_frames_promoted_multi_pane: u64,
    actor_frame_partial_promotion_violations: u64,
    visibility_changes: u64,
    max_visible_panes: usize,
    raster_scale: f32,
    /// Revision of every font-selection/configuration input used by shaping.
    /// Font configuration is currently immutable after bridge construction;
    /// a future mutation must advance this before admitting more layouts.
    font_context_revision: u64,
}

fn replace_facet_target(state: &mut GraphicsState, actor: &ActorScope, next_frame_ns: Option<u64>) {
    if let Some(deadline) = next_frame_ns {
        state.next_facet_targets.insert(actor.clone(), deadline);
    } else {
        state.next_facet_targets.remove(actor);
    }
}

fn take_facet_targets(state: &mut GraphicsState) -> Vec<(ActorScope, u64)> {
    let due = state
        .next_facet_targets
        .iter()
        .map(|(actor, deadline)| {
            let visible = state
                .actor_panes
                .get(actor)
                .is_some_and(|owned| owned.iter().any(|pane| state.visible_panes.contains(pane)));
            (actor.clone(), *deadline, visible)
        })
        .collect::<Vec<_>>();
    for (actor, _deadline, _visible) in &due {
        state.next_facet_targets.remove(actor);
    }
    due.into_iter()
        .filter_map(|(actor, deadline, visible)| visible.then_some((actor, deadline)))
        .collect()
}

#[derive(Debug)]
struct PendingGraphicsActorFrame {
    actor: ActorScope,
    token: u64,
    revision: u64,
    scenes: BTreeMap<u64, PaneScene>,
    ready_notified: bool,
    required_panes: Option<BTreeSet<u64>>,
    blocker_epochs: BTreeSet<u64>,
    facet_receipt: Option<PreparedFacetRender>,
    facet_atomic: bool,
}

struct DeferredActorFrameStage {
    token: u64,
    blocker_epochs: BTreeSet<u64>,
    superseded_revisions: Vec<u64>,
}

struct ReferencedResources {
    images: BTreeMap<u64, ImageResource>,
    text_layouts: BTreeMap<u64, TextLayoutResource>,
    glyphs: BTreeMap<GlyphCacheKey, GlyphImageResource>,
    ready: bool,
}

fn scene_references_resource(scene: &PaneScene, resource: u64) -> bool {
    scene.packet.commands.iter().any(|command| match command {
        Command::DrawImage { image, .. } => *image == resource,
        Command::DrawTextLayout { layout, .. } => *layout == resource,
        _ => false,
    })
}

fn resource_is_referenced(state: &GraphicsState, resource: u64) -> bool {
    state
        .panes
        .values()
        .chain(state.deferred_panes.values())
        .chain(state.staged_panes.values())
        .chain(
            state
                .pending_actor_frames
                .values()
                .flat_map(|frame| frame.scenes.values()),
        )
        .chain(
            state
                .deferred_actor_frames
                .values()
                .flat_map(|frame| frame.scenes.values()),
        )
        .any(|scene| scene_references_resource(scene, resource))
}

fn record_complete_state_watermark(state: &mut GraphicsState, pane: u64, watermark: u64) {
    state
        .latest_complete_state_watermarks
        .entry(pane)
        .and_modify(|latest| *latest = (*latest).max(watermark))
        .or_insert(watermark);
}

fn validate_state_watermark(
    state: &GraphicsState,
    pane: u64,
    watermark: Option<u64>,
) -> Result<(), PresentationError> {
    let Some(watermark) = watermark else {
        return Ok(());
    };
    let Some(latest_complete) = state.latest_complete_state_watermarks.get(&pane) else {
        return Err(PresentationError::InvalidPacket);
    };
    if watermark > *latest_complete {
        return Err(PresentationError::InvalidPacket);
    }
    Ok(())
}

fn validate_actor_frame_before_preparation<T>(
    draws: Vec<ActorFrameDraw>,
    mut validate: impl FnMut(&ActorFrameDraw) -> Result<(), PresentationError>,
    mut prepare: impl FnMut(ActorFrameDraw) -> Result<T, PresentationError>,
) -> Result<Vec<T>, PresentationError> {
    for draw in &draws {
        validate(draw)?;
    }
    draws.into_iter().map(&mut prepare).collect()
}

fn ready_actor_frame_notifications(
    state: &mut GraphicsState,
) -> Vec<(u64, u64, Vec<ActorFrameGraphicsChange>)> {
    let revisions = state
        .pending_actor_frames
        .keys()
        .copied()
        .collect::<Vec<_>>();
    let mut notifications = Vec::new();
    for revision in revisions {
        let Some(frame) = state.pending_actor_frames.get(&revision) else {
            continue;
        };
        if frame.ready_notified {
            continue;
        }
        let visible = if let Some(required) = &frame.required_panes {
            let drawable = if frame.facet_atomic {
                // A facet response is one complete owned-pane batch, including
                // hidden members. Prepare and commit that batch intact when
                // any member is visible; requiring every member to be visible
                // deadlocks a multi-pane actor after one pane is hidden.
                !required.is_disjoint(&state.visible_panes)
            } else {
                required.is_subset(&state.visible_panes)
            };
            if !drawable {
                continue;
            }
            required
                .iter()
                .filter_map(|pane| frame.scenes.get_key_value(pane))
                .collect::<Vec<_>>()
        } else {
            frame
                .scenes
                .iter()
                .filter(|(pane, _scene)| state.visible_panes.contains(pane))
                .collect::<Vec<_>>()
        };
        if visible.is_empty()
            || visible.iter().any(|(pane, scene)| {
                referenced_resources(state, **pane, scene.generation, &scene.packet)
                    .map_or(true, |resources| !resources.ready)
            })
        {
            continue;
        }
        let changes = visible
            .into_iter()
            .map(|(pane, scene)| ActorFrameGraphicsChange {
                pane: *pane,
                sequence: scene.sequence,
                resize_epoch: scene.resize_epoch,
                terminal_input_sequence: scene.terminal_input_sequence,
            })
            .collect::<Vec<_>>();
        if std::env::var_os("BROOD_TRACE_PLUGIN_GRAPHICS").is_some() {
            eprintln!(
                "facet bridge ready actor={} token={} revision={} panes={:?}",
                frame.actor.name,
                frame.token,
                frame.revision,
                frame.scenes.keys().collect::<Vec<_>>()
            );
        }
        notifications.push((frame.token, frame.revision, changes));
        if let Some(frame) = state.pending_actor_frames.get_mut(&revision) {
            frame.ready_notified = true;
            state.actor_frames_ready = state.actor_frames_ready.saturating_add(1);
        }
    }
    notifications
}

fn publish_actor_frame_notifications(
    proxy: &EventLoopProxy<ClientEvent>,
    notifications: Vec<(u64, u64, Vec<ActorFrameGraphicsChange>)>,
) {
    for (token, actor_frame_revision, changes) in notifications {
        if proxy
            .send_event(ClientEvent::PluginActorFrameChanged {
                token,
                actor_frame_revision,
                changes,
                ready_at: Instant::now(),
            })
            .is_err()
        {
            eprintln!(
                "failed to publish ready plugin actor frame token={token} revision={actor_frame_revision}"
            );
        }
    }
}

fn actor_frame_snapshots_from_state(
    state: &GraphicsState,
    revision: u64,
    panes: impl IntoIterator<Item = u64>,
) -> Result<Option<Vec<GraphicsSnapshot>>> {
    let Some(frame) = state.pending_actor_frames.get(&revision) else {
        return Ok(None);
    };
    panes
        .into_iter()
        .map(|pane| {
            let scene = frame
                .scenes
                .get(&pane)
                .context("actor frame pane is unavailable")?;
            let resources = referenced_resources(state, pane, scene.generation, &scene.packet)
                .map_err(|_error| anyhow!("actor frame lost a referenced resource"))?;
            anyhow::ensure!(resources.ready, "actor frame pane resources are not ready");
            Ok(GraphicsSnapshot {
                facet_extent: scene.facet_frame.as_ref().map(|frame| {
                    (
                        frame.geometry().physical_width,
                        frame.geometry().physical_height,
                    )
                }),
                pane,
                sequence: scene.sequence,
                resize_epoch: scene.resize_epoch,
                packet: scene.packet.clone(),
                images: resources.images,
                text_layouts: resources.text_layouts,
                glyphs: resources.glyphs,
                facet_frame_lease: scene.facet_frame.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn promote_actor_frame_in_state(
    state: &mut GraphicsState,
    revision: u64,
    panes: &[u64],
) -> Result<(ActorFramePromotion, Option<u64>)> {
    let Some(mut frame) = state.pending_actor_frames.remove(&revision) else {
        return Ok((ActorFramePromotion::Stale, None));
    };
    let promoted_panes = panes.iter().copied().collect::<BTreeSet<_>>();
    if promoted_panes.len() != panes.len()
        || promoted_panes
            .iter()
            .any(|pane| !frame.scenes.contains_key(pane))
        || frame
            .required_panes
            .as_ref()
            .is_some_and(|required| required != &promoted_panes)
    {
        state.actor_frame_partial_promotion_violations = state
            .actor_frame_partial_promotion_violations
            .saturating_add(1);
        state.pending_actor_frames.insert(revision, frame);
        anyhow::bail!("actor frame promotion pane set is invalid");
    }
    if let Some(receipt) = &frame.facet_receipt {
        let candidate = receipt
            .with_graphics(|graphics| {
                (
                    graphics.token().candidate,
                    graphics.token().snapshot_revision,
                )
            })
            .ok();
        let Ok(next_frame_ns) = receipt.validate_current().and_then(|()| receipt.commit()) else {
            // The scheduler accepted this frame before native promotion. A
            // rejected interaction receipt consumes the bridge frame, so its
            // accepted token must be settled before replacement demand can
            // arm. A replayed notification for an already-consumed revision
            // takes the missing-frame branch above and has no token to cancel.
            return Ok((ActorFramePromotion::Stale, Some(frame.token)));
        };
        replace_facet_target(state, &frame.actor, next_frame_ns);
        tracing::info!(
            "facet native pair committed route=ordinary actor={} generation={} native_revision={} candidate={} snapshot={} pane_count={} panes={promoted_panes:?}",
            frame.actor.name,
            frame.actor.generation,
            revision,
            candidate.map_or(0, |identity| identity.0),
            candidate.map_or(0, |identity| identity.1),
            promoted_panes.len(),
        );
    }
    let actor = frame.actor.clone();
    let token = frame.token;
    let resize_epochs = panes
        .iter()
        .filter_map(|pane| frame.scenes.get(pane).and_then(|scene| scene.resize_epoch))
        .collect::<BTreeSet<_>>();
    for pane in panes {
        let scene = frame
            .scenes
            .remove(pane)
            .context("actor frame pane is unavailable")?;
        if let Some(epoch) = scene.resize_epoch {
            state.staged_panes.insert((*pane, epoch), scene);
        } else if state.active_resize_epochs.contains_key(pane) {
            state.deferred_panes.insert(*pane, scene);
        } else {
            state.panes.insert(*pane, scene);
        }
    }
    if !frame.scenes.is_empty() {
        frame.ready_notified = false;
        frame.required_panes = None;
        state.pending_actor_frames.insert(revision, frame);
    }
    state.actor_frames_promoted = state.actor_frames_promoted.saturating_add(1);
    if panes.len() >= 2 {
        state.actor_frames_promoted_multi_pane =
            state.actor_frames_promoted_multi_pane.saturating_add(1);
    }
    tracing::info!(
        "render scheduling actor-frame promoted actor={} generation={} token={} revision={} pane_count={} panes={panes:?} resize_epochs={resize_epochs:?}",
        actor.name,
        actor.generation,
        token,
        revision,
        panes.len(),
    );
    Ok((ActorFramePromotion::Claimed, None))
}

fn claim_actor_frame_for_preparation_in_state(
    state: &mut GraphicsState,
    revision: u64,
    panes: &[u64],
) -> Result<ActorFramePreparationClaim> {
    let Some(frame) = state.pending_actor_frames.get(&revision) else {
        return Ok(ActorFramePreparationClaim::Stale);
    };
    let claimed_panes = panes.iter().copied().collect::<BTreeSet<_>>();
    if claimed_panes.len() != panes.len()
        || claimed_panes
            .iter()
            .any(|pane| !frame.scenes.contains_key(pane))
        || frame
            .required_panes
            .as_ref()
            .is_some_and(|required| required != &claimed_panes)
    {
        state.actor_frame_partial_promotion_violations = state
            .actor_frame_partial_promotion_violations
            .saturating_add(1);
        anyhow::bail!("actor frame preparation claim pane set is invalid");
    }
    let blocker_epochs = frame
        .scenes
        .iter()
        .filter_map(|(pane, scene)| {
            scene
                .resize_epoch
                .or_else(|| state.active_resize_epochs.get(pane).copied())
        })
        .collect::<BTreeSet<_>>();
    if blocker_epochs.is_empty() {
        return Ok(ActorFramePreparationClaim::Prepare);
    }
    let frame = state
        .pending_actor_frames
        .get_mut(&revision)
        .context("blocked actor frame disappeared during preparation claim")?;
    frame.required_panes = Some(claimed_panes);
    frame.blocker_epochs = blocker_epochs;
    Ok(ActorFramePreparationClaim::PrepareDeferred)
}

fn defer_prepared_actor_frame_in_state(
    state: &mut GraphicsState,
    revision: u64,
    panes: &[u64],
) -> Result<Option<DeferredActorFrameStage>> {
    let Some(frame) = state.pending_actor_frames.get(&revision) else {
        return Ok(None);
    };
    let claimed = panes.iter().copied().collect::<BTreeSet<_>>();
    anyhow::ensure!(
        frame.required_panes.as_ref() == Some(&claimed) && !frame.blocker_epochs.is_empty(),
        "prepared deferred actor frame claim changed"
    );
    let frame = state
        .pending_actor_frames
        .remove(&revision)
        .context("prepared deferred actor frame disappeared")?;
    let token = frame.token;
    let blocker_epochs = frame.blocker_epochs.clone();
    let actor = frame.actor.clone();
    let superseded = state
        .deferred_actor_frames
        .iter()
        .filter_map(|(revision, deferred)| (deferred.actor == actor).then_some(*revision))
        .collect::<Vec<_>>();
    state
        .deferred_actor_frames
        .retain(|_revision, deferred| deferred.actor != actor);
    state.deferred_actor_frames.insert(revision, frame);
    Ok(Some(DeferredActorFrameStage {
        token,
        blocker_epochs,
        superseded_revisions: superseded,
    }))
}

fn release_deferred_actor_frames_for_epoch(
    state: &mut GraphicsState,
    resize_epoch: u64,
    resolved_panes: &BTreeSet<u64>,
    promote_tagged: bool,
) -> Vec<(u64, u64, Option<u64>)> {
    let mut released = Vec::new();
    let revisions = state
        .deferred_actor_frames
        .iter()
        .filter_map(|(revision, frame)| {
            (frame.blocker_epochs.contains(&resize_epoch)
                && frame
                    .scenes
                    .keys()
                    .any(|pane| resolved_panes.contains(pane)))
            .then_some(*revision)
        })
        .collect::<Vec<_>>();
    for revision in revisions {
        let Some(mut frame) = state.deferred_actor_frames.remove(&revision) else {
            continue;
        };
        frame.blocker_epochs.remove(&resize_epoch);
        // This fallback is used by resize cleanup without a complete prepared
        // native texture set. A facet frame must remain paired with interaction
        // authority, so cleanup discards it instead of exposing its CPU scene.
        if frame.facet_atomic {
            continue;
        }
        if promote_tagged {
            for scene in frame.scenes.values_mut() {
                if scene.resize_epoch == Some(resize_epoch) {
                    scene.resize_epoch = None;
                }
            }
        } else {
            // Tagged content was never presented and is invalid after an abort
            // or degraded completion. Ordinary members, including panes not in
            // the wait set, remain one atomic recovery frame.
            frame
                .scenes
                .retain(|_pane, scene| scene.resize_epoch != Some(resize_epoch));
        }
        if !frame.blocker_epochs.is_empty() {
            state.deferred_actor_frames.insert(revision, frame);
            continue;
        }
        let unresolved_tags = frame
            .scenes
            .values()
            .filter_map(|scene| scene.resize_epoch)
            .collect::<BTreeSet<_>>();
        if !unresolved_tags.is_empty() {
            frame.blocker_epochs = unresolved_tags;
            state.deferred_actor_frames.insert(revision, frame);
            continue;
        }
        if frame.scenes.is_empty() {
            continue;
        }
        if frame.facet_atomic
            && frame.scenes.iter().any(|(pane, scene)| {
                state
                    .panes
                    .get(pane)
                    .is_some_and(|visible| scene.sequence <= visible.sequence)
            })
        {
            continue;
        }
        for (pane, scene) in frame.scenes {
            if state
                .panes
                .get(&pane)
                .is_none_or(|visible| scene.sequence > visible.sequence)
            {
                released.push((pane, scene.sequence, scene.terminal_input_sequence));
                state.panes.insert(pane, scene);
            }
        }
    }
    released
}

fn deferred_actor_frame_resolution_plan_in_state(
    state: &mut GraphicsState,
    resize_epoch: u64,
    promote_tagged: bool,
    promote_prepared_facets: bool,
) -> DeferredActorFrameResolutionPlan {
    let mut plan = DeferredActorFrameResolutionPlan::default();
    let revisions = state
        .deferred_actor_frames
        .keys()
        .copied()
        .collect::<Vec<_>>();
    for revision in revisions {
        let Some(mut frame) = state.deferred_actor_frames.remove(&revision) else {
            continue;
        };
        if !frame.blocker_epochs.remove(&resize_epoch) {
            plan.retain.insert(revision);
            state.deferred_actor_frames.insert(revision, frame);
            continue;
        }
        // Degraded native promotion can still install a complete, validated
        // facet batch. An unrelated managed-client timeout must not discard
        // that exact output: a newly exposed pane may have only a blank fallback.
        // Abort and cleanup never opt in because no native texture set commits.
        if promote_tagged || (promote_prepared_facets && frame.facet_atomic) {
            for scene in frame.scenes.values_mut() {
                if scene.resize_epoch == Some(resize_epoch) {
                    scene.resize_epoch = None;
                }
            }
        } else {
            if frame.facet_atomic {
                plan.cancelled_tokens.push(frame.token);
                plan.commit.insert(revision, BTreeMap::new());
                continue;
            }
            frame
                .scenes
                .retain(|_pane, scene| scene.resize_epoch != Some(resize_epoch));
        }
        if !frame.blocker_epochs.is_empty() {
            plan.retain.insert(revision);
            state.deferred_actor_frames.insert(revision, frame);
            continue;
        }
        let unresolved_tags = frame
            .scenes
            .values()
            .filter_map(|scene| scene.resize_epoch)
            .collect::<BTreeSet<_>>();
        if !unresolved_tags.is_empty() {
            frame.blocker_epochs = unresolved_tags;
            plan.retain.insert(revision);
            state.deferred_actor_frames.insert(revision, frame);
            continue;
        }
        let facet_member_is_stale = frame.facet_atomic
            && frame.scenes.iter().any(|(pane, scene)| {
                state
                    .panes
                    .get(pane)
                    .map(|visible| visible.sequence)
                    .into_iter()
                    .chain(state.staged_panes.iter().filter_map(
                        |((staged_pane, _epoch), staged)| {
                            (*staged_pane == *pane).then_some(staged.sequence)
                        },
                    ))
                    .max()
                    .is_some_and(|latest| scene.sequence <= latest)
            });
        if facet_member_is_stale {
            plan.cancelled_tokens.push(frame.token);
            plan.commit.insert(revision, BTreeMap::new());
            continue;
        }
        if let Some(receipt) = &frame.facet_receipt {
            let candidate = receipt
                .with_graphics(|graphics| {
                    (
                        graphics.token().candidate,
                        graphics.token().snapshot_revision,
                    )
                })
                .ok();
            let Ok(next_frame_ns) = receipt.validate_current().and_then(|()| receipt.commit())
            else {
                plan.cancelled_tokens.push(frame.token);
                plan.commit.insert(revision, BTreeMap::new());
                continue;
            };
            replace_facet_target(state, &frame.actor, next_frame_ns);
            let panes = frame.scenes.keys().copied().collect::<BTreeSet<_>>();
            tracing::info!(
                "facet native pair committed route=synchronized actor={} generation={} native_revision={} candidate={} snapshot={} resize_epoch={} pane_count={} panes={panes:?}",
                frame.actor.name,
                frame.actor.generation,
                revision,
                candidate.map_or(0, |identity| identity.0),
                candidate.map_or(0, |identity| identity.1),
                resize_epoch,
                panes.len(),
            );
        }
        let mut members = BTreeMap::new();
        let mut correlations = Vec::new();
        for (pane, scene) in frame.scenes {
            let latest_sequence =
                state
                    .panes
                    .get(&pane)
                    .map(|visible| visible.sequence)
                    .into_iter()
                    .chain(state.staged_panes.iter().filter_map(
                        |((staged_pane, _epoch), staged)| {
                            (*staged_pane == pane).then_some(staged.sequence)
                        },
                    ))
                    .max();
            if latest_sequence.is_none_or(|latest| scene.sequence > latest) {
                members.insert(pane, scene.sequence);
                correlations.push((pane, scene.sequence, scene.terminal_input_sequence));
                state.panes.insert(pane, scene);
            }
        }
        state
            .settled_actor_frame_correlations
            .entry(resize_epoch)
            .or_default()
            .extend(correlations);
        plan.commit.insert(revision, members);
    }
    plan
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
    let resolved_panes = panes.iter().copied().collect::<BTreeSet<_>>();
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
    deferred.extend(release_deferred_actor_frames_for_epoch(
        state,
        resize_epoch,
        &resolved_panes,
        false,
    ));
    deferred.extend(
        state
            .settled_actor_frame_correlations
            .remove(&resize_epoch)
            .unwrap_or_default(),
    );
    deferred
}

fn retire_pending_actor_frames_for_epoch(state: &mut GraphicsState, resize_epoch: u64) -> Vec<u64> {
    let mut cancelled_tokens = Vec::new();
    state.pending_actor_frames.retain(|_revision, frame| {
        let retired = frame
            .scenes
            .values()
            .any(|scene| scene.resize_epoch == Some(resize_epoch));
        if retired {
            cancelled_tokens.push(frame.token);
        }
        !retired
    });
    cancelled_tokens
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
    let resolved_panes = panes.iter().copied().collect::<BTreeSet<_>>();
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
    deferred.extend(release_deferred_actor_frames_for_epoch(
        state,
        resize_epoch,
        &resolved_panes,
        promote_tagged,
    ));
    deferred.extend(
        state
            .settled_actor_frame_correlations
            .remove(&resize_epoch)
            .unwrap_or_default(),
    );
    deferred
}

impl PluginGraphicsBridge {
    pub fn new(proxy: EventLoopProxy<ClientEvent>) -> Self {
        let shared = Arc::new(Mutex::new(GraphicsState {
            raster_scale: DEFAULT_RASTER_SCALE,
            font_context_revision: 1,
            ..GraphicsState::default()
        }));
        let text_system = Arc::new(Mutex::new(TextSystem {
            fonts: FontSystem::new(),
            swash: SwashCache::new(),
            glyphs: BTreeMap::new(),
            glyph_cache_bytes: 0,
        }));
        let glyph_raster_tx =
            start_glyph_raster_worker(Arc::clone(&shared), Arc::clone(&text_system), proxy.clone());
        Self {
            shared,
            text_system,
            glyph_raster_tx,
            proxy,
            render_scheduler: Arc::new(Mutex::new(RenderScheduler::default())),
            render_clock_origin: Instant::now(),
            host_tail_ns: Arc::new(AtomicU64::new(COLD_HOST_COST_NS)),
            winit_redraw_hop_ns: Arc::new(AtomicU64::new(COLD_WINIT_REDRAW_HOP_NS)),
        }
    }

    /// Keep glyph bitmaps in device pixels. Canvas geometry remains logical,
    /// so changing output scale only requires replacing the raster resources
    /// behind the existing layout handles.
    pub fn set_raster_scale(&self, scale: f32) {
        let scale = sanitize_raster_scale(scale);
        let changed = self.shared.lock().is_ok_and(|mut state| {
            if state.raster_scale.to_bits() == scale.to_bits() {
                return false;
            }
            state.raster_scale = scale;
            for entry in state.text_layouts.values_mut() {
                entry.raster_scale = scale;
            }
            true
        });
        if changed
            && self
                .glyph_raster_tx
                .send(GlyphRasterWork::ScaleChanged(scale))
                .is_err()
        {
            eprintln!("could not queue glyph reraster for output scale {scale:.3}");
        }
    }

    pub fn actor_host(
        &self,
        actor: impl Into<String>,
        panes: impl IntoIterator<Item = u64>,
    ) -> Arc<ActorPresentationHost> {
        Arc::new(ActorPresentationHost {
            actor: actor.into(),
            panes: panes.into_iter().collect(),
            generation: AtomicU64::new(0),
            retired_generation: AtomicU64::new(0),
            bridge: self.clone(),
        })
    }

    pub fn render_now_ns(&self) -> u64 {
        u64::try_from(self.render_clock_origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    pub fn service_render_scheduler(
        &self,
        predictor: &mut FramePredictor,
    ) -> Result<RenderSchedulerService> {
        let now_ns = self.render_now_ns();
        let host_cost_ns = self.host_tail_ns.load(Ordering::Acquire);
        let redraw_hop_ns = self.winit_redraw_hop_ns.load(Ordering::Acquire);
        let facet_targets = {
            let mut state = self
                .shared
                .lock()
                .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
            take_facet_targets(&mut state)
        };
        let mut scheduler = self
            .render_scheduler
            .lock()
            .map_err(|_poisoned| anyhow!("render scheduler is poisoned"))?;
        for (actor, target_ns) in facet_targets {
            scheduler.request_render_at(&actor, target_ns);
        }
        for missed in scheduler.drain_deadline_results() {
            predictor.note_deadline_result(missed);
        }
        scheduler.arm_all(now_ns, predictor, host_cost_ns);
        let request_redraw_target_ns =
            scheduler.request_due_redraw(now_ns, host_cost_ns, redraw_hop_ns);
        let hints = scheduler
            .dispatch_due(now_ns)
            .into_iter()
            .map(|(actor, hint)| {
                (
                    actor,
                    GuestRenderHint {
                        token: GuestRenderToken(hint.token.0),
                        target_presentation_time_ns: hint.target_presentation_time_ns,
                        submission_deadline_ns: hint.submission_deadline_ns,
                        relevant_panes: hint.relevant_panes,
                        event_watermark: hint.event_watermark,
                    },
                )
            })
            .collect();
        let composition_tokens = scheduler
            .release_ready_composition()
            .into_iter()
            .map(|token| token.0)
            .collect();
        drop(scheduler);
        Ok(RenderSchedulerService {
            hints,
            composition_tokens,
            request_redraw_target_ns,
        })
    }

    pub fn note_host_tail(&self, measured_ns: u64) {
        let measured_ns = measured_ns.clamp(1, 50_000_000);
        let _updated =
            self.host_tail_ns
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    Some(if measured_ns > current {
                        measured_ns
                    } else {
                        current
                            .saturating_mul(7)
                            .saturating_add(measured_ns)
                            .saturating_div(8)
                            .max(1)
                    })
                });
    }

    pub fn note_winit_redraw_hop(&self, measured_ns: u64) {
        let measured_ns = measured_ns.clamp(1, MAX_WINIT_REDRAW_HOP_NS);
        let _updated =
            self.winit_redraw_hop_ns
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    Some(if measured_ns > current {
                        measured_ns
                    } else {
                        current
                            .saturating_mul(7)
                            .saturating_add(measured_ns)
                            .saturating_div(8)
                            .max(1)
                    })
                });
    }

    pub fn next_render_deadline(&self) -> Result<Option<Instant>> {
        let scheduler = self
            .render_scheduler
            .lock()
            .map_err(|_poisoned| anyhow!("render scheduler is poisoned"))?;
        let host_cost_ns = self.host_tail_ns.load(Ordering::Acquire);
        let redraw_hop_ns = self.winit_redraw_hop_ns.load(Ordering::Acquire);
        Ok(scheduler
            .next_deadline_ns()
            .into_iter()
            .chain(scheduler.next_redraw_request_deadline_ns(host_cost_ns, redraw_hop_ns))
            .min()
            .and_then(|deadline| {
                self.render_clock_origin
                    .checked_add(std::time::Duration::from_nanos(deadline))
            }))
    }

    pub fn mark_graphics_ready(
        &self,
        token: u64,
        panes: impl IntoIterator<Item = u64>,
    ) -> CompositionRelease {
        let now = self.render_now_ns();
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            for pane in panes {
                let token = RenderToken(token);
                if scheduler.token_expects_graphics_ready(token, pane) {
                    scheduler.mark_graphics_ready(token, pane, now);
                }
            }
            let redraw_hop_ns = self.winit_redraw_hop_ns.load(Ordering::Acquire);
            let tokens = scheduler
                .release_ready_composition()
                .into_iter()
                .map(|token| token.0)
                .collect::<Vec<_>>();
            let target_redraw_consumed = tokens
                .iter()
                .any(|token| scheduler.token_target_redraw_consumed(RenderToken(*token)));
            return CompositionRelease {
                tokens,
                request_redraw_target_ns: if target_redraw_consumed {
                    scheduler.token_target_ns(RenderToken(token))
                } else {
                    scheduler.request_due_redraw(
                        now,
                        self.host_tail_ns.load(Ordering::Acquire),
                        redraw_hop_ns,
                    )
                },
            };
        }
        CompositionRelease {
            tokens: Vec::new(),
            request_redraw_target_ns: None,
        }
    }

    pub fn mark_winit_redraw_started(&self) -> Option<u64> {
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            return Some(scheduler.mark_redraw_consumed());
        }
        None
    }

    pub fn composable_tokens_through(
        &self,
        tokens: impl IntoIterator<Item = u64>,
        target_ns: u64,
    ) -> Vec<u64> {
        let Ok(scheduler) = self.render_scheduler.lock() else {
            return Vec::new();
        };
        tokens
            .into_iter()
            .filter(|token| {
                scheduler
                    .token_target_ns(RenderToken(*token))
                    .is_some_and(|token_target_ns| token_target_ns <= target_ns)
            })
            .collect()
    }

    pub fn mark_frames_composed(
        &self,
        tokens: impl IntoIterator<Item = u64>,
        composed_at: Instant,
    ) {
        let composed_ns = u64::try_from(
            composed_at
                .saturating_duration_since(self.render_clock_origin)
                .as_nanos(),
        )
        .unwrap_or(u64::MAX);
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            for token in tokens {
                scheduler.mark_composed(RenderToken(token), composed_ns);
            }
        }
    }

    pub fn mark_frame_presented(
        &self,
        tokens: impl IntoIterator<Item = u64>,
        submitted_at: Instant,
        presented_at: Instant,
        estimated: bool,
    ) -> bool {
        let to_ns = |instant: Instant| {
            u64::try_from(
                instant
                    .saturating_duration_since(self.render_clock_origin)
                    .as_nanos(),
            )
            .unwrap_or(u64::MAX)
        };
        let submitted_ns = to_ns(submitted_at);
        let presented_ns = to_ns(presented_at);
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            let warmup_before = scheduler.warmup_opportunities();
            for token in tokens {
                let token = RenderToken(token);
                scheduler.mark_gpu_submitted(token, submitted_ns);
                scheduler.mark_presented(token, presented_ns, estimated);
            }
            return warmup_before < DIAGNOSTIC_WARMUP_OPPORTUNITIES
                && scheduler.warmup_opportunities() == DIAGNOSTIC_WARMUP_OPPORTUNITIES;
        }
        false
    }

    pub fn cancel_accepted_tokens(&self, tokens: impl IntoIterator<Item = u64>) {
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            for token in tokens {
                scheduler.cancel_accepted(RenderToken(token));
            }
        }
    }

    pub fn admit_actor_event(&self, actor: &ActorScope, key_input: bool) -> Option<u64> {
        let now = self.render_now_ns();
        self.render_scheduler.lock().ok().and_then(|mut scheduler| {
            let watermark = scheduler.admit_event(actor, false)?;
            if key_input {
                scheduler.note_key_input(actor, watermark, now);
            }
            Some(watermark)
        })
    }

    pub fn complete_facet_input(&self, actor: &ActorScope, event_watermark: u64) {
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            scheduler.mark_input_correlation_ready(actor, event_watermark);
        }
    }

    pub fn complete_facet_publication(&self, actor: &ActorScope, event_watermark: u64) {
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            scheduler.mark_input_correlations_ready_through(actor, event_watermark);
        }
    }

    pub fn complete_actor_turn(
        &self,
        actor: &ActorScope,
        event_watermark: Option<u64>,
        render_token: Option<u64>,
    ) {
        let now = self.render_now_ns();
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            if let Some(watermark) = event_watermark {
                scheduler.mark_reduced_through(actor, watermark);
            }
            if let Some(token) = render_token
                && scheduler.token_belongs_to(actor, RenderToken(token))
            {
                scheduler.abandon(actor, now);
            }
        }
        drop(self.proxy.send_event(ClientEvent::RenderSchedulingChanged));
    }

    pub fn set_visible_panes(
        &self,
        panes: impl IntoIterator<Item = u64>,
        composition_panes: impl IntoIterator<Item = u64>,
    ) -> Result<()> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        let panes = panes.into_iter().collect::<BTreeSet<_>>();
        let composition_panes = composition_panes.into_iter().collect::<BTreeSet<_>>();
        if state.visible_panes != panes {
            state.visible_panes = panes;
            state.max_visible_panes = state.max_visible_panes.max(state.visible_panes.len());
            state.visibility_changes = state.visibility_changes.saturating_add(1);
            for frame in state.pending_actor_frames.values_mut() {
                frame.ready_notified = false;
            }
        }
        let notifications = ready_actor_frame_notifications(&mut state);
        let actor_visibility = state
            .actor_panes
            .iter()
            .map(|(actor, owned)| {
                (
                    actor.clone(),
                    owned
                        .intersection(&composition_panes)
                        .copied()
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        drop(state);
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            for (actor, relevant) in actor_visibility {
                scheduler.set_relevant_panes(&actor, relevant);
            }
        }
        publish_actor_frame_notifications(&self.proxy, notifications);
        Ok(())
    }

    pub fn renderer_recreated(&self) -> Result<()> {
        // The renderer generation changes, but the Winit event loop, window,
        // compositor path, and established encode/submit tail remain the same
        // scheduling domain. Preserve both estimates so recovered targets are
        // prearmed with trained costs instead of relearning through misses.
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            scheduler.reset_redraw_frontier();
        }
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        for frame in state.pending_actor_frames.values_mut() {
            frame.ready_notified = false;
        }
        let notifications = ready_actor_frame_notifications(&mut state);
        drop(state);
        publish_actor_frame_notifications(&self.proxy, notifications);
        Ok(())
    }

    pub fn actor_frame_snapshots(
        &self,
        revision: u64,
        panes: impl IntoIterator<Item = u64>,
    ) -> Result<Option<Vec<GraphicsSnapshot>>> {
        let state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        actor_frame_snapshots_from_state(&state, revision, panes)
    }

    /// Claim a complete actor frame before any renderer-side packet or
    /// resource preparation begins. An ordinary draw for a pane currently
    /// held by a synchronized-resize barrier retires the complete frame; only
    /// its blocked ordinary panes remain deferred for abort/post-present
    /// recovery.
    pub fn claim_actor_frame_for_preparation(
        &self,
        revision: u64,
        panes: &[u64],
    ) -> Result<ActorFramePreparationClaim> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        claim_actor_frame_for_preparation_in_state(&mut state, revision, panes)
    }

    pub fn defer_prepared_actor_frame(
        &self,
        revision: u64,
        panes: &[u64],
    ) -> Result<Option<(BTreeSet<u64>, Vec<u64>)>> {
        let deferred = {
            let mut state = self
                .shared
                .lock()
                .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
            defer_prepared_actor_frame_in_state(&mut state, revision, panes)?
        };
        let Some(deferred) = deferred else {
            return Ok(None);
        };
        self.cancel_accepted_tokens([deferred.token]);
        drop(self.proxy.send_event(ClientEvent::RenderSchedulingChanged));
        Ok(Some((
            deferred.blocker_epochs,
            deferred.superseded_revisions,
        )))
    }

    pub fn promote_actor_frame(&self, revision: u64, panes: &[u64]) -> Result<ActorFramePromotion> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        let (promotion, rejected_token) =
            promote_actor_frame_in_state(&mut state, revision, panes)?;
        drop(state);
        if let Some(token) = rejected_token {
            self.cancel_accepted_tokens([token]);
        }
        Ok(promotion)
    }

    /// Retire an actor frame whose complete atomic batch can no longer be
    /// presented. This is used when asynchronous preparation resumes after a
    /// resize epoch in the batch has already been promoted or superseded.
    pub fn discard_actor_frame(&self, revision: u64) -> Result<bool> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        let token = state
            .pending_actor_frames
            .remove(&revision)
            .or_else(|| state.deferred_actor_frames.remove(&revision))
            .map(|frame| frame.token);
        drop(state);
        if let Some(token) = token {
            self.cancel_accepted_tokens([token]);
            // Cancellation can expose a newer relevant-pane or layout
            // revision that arrived while this accepted frame was awaiting
            // native preparation. Wake the event loop so that demand is
            // armed without waiting for unrelated input or polling.
            drop(self.proxy.send_event(ClientEvent::RenderSchedulingChanged));
            return Ok(true);
        }
        Ok(false)
    }

    pub fn facet_receipt(&self, revision: u64) -> Result<Option<PreparedFacetRender>> {
        let state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        Ok(state
            .pending_actor_frames
            .get(&revision)
            .or_else(|| state.deferred_actor_frames.get(&revision))
            .and_then(|frame| frame.facet_receipt.clone()))
    }

    pub fn expect_terminal_snapshot(&self, pane: u64, input_sequence: u64) -> Result<()> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        record_complete_state_watermark(&mut state, pane, input_sequence);
        drop(state);
        Ok(())
    }

    /// Add panes to the current epoch without revoking panes which an
    /// independent resolved branch already admitted.
    pub fn admit_resize_epoch_panes(&self, resize_epoch: u64, panes: &[u64]) -> Result<()> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        anyhow::ensure!(
            state
                .active_resize_epochs
                .values()
                .all(|active| *active == resize_epoch),
            "plugin graphics has panes admitted for another resize epoch"
        );
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
            facet_extent: scene.facet_frame.as_ref().map(|frame| {
                (
                    frame.geometry().physical_width,
                    frame.geometry().physical_height,
                )
            }),
            pane,
            sequence: scene.sequence,
            resize_epoch: None,
            packet: scene.packet.clone(),
            images: resources.images,
            text_layouts: resources.text_layouts,
            glyphs: resources.glyphs,
            facet_frame_lease: scene.facet_frame.clone(),
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
            facet_extent: scene.facet_frame.as_ref().map(|frame| {
                (
                    frame.geometry().physical_width,
                    frame.geometry().physical_height,
                )
            }),
            pane,
            sequence: scene.sequence,
            resize_epoch: Some(resize_epoch),
            packet: scene.packet.clone(),
            images: resources.images,
            text_layouts: resources.text_layouts,
            glyphs: resources.glyphs,
            facet_frame_lease: scene.facet_frame.clone(),
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
        let cancelled_tokens = retire_pending_actor_frames_for_epoch(&mut state, resize_epoch);
        let discarded = before.saturating_sub(state.staged_panes.len());
        drop(state);
        self.cancel_accepted_tokens(cancelled_tokens);
        Ok(discarded)
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

    pub fn deferred_actor_frame_resolution_plan(
        &self,
        resize_epoch: u64,
        promote_tagged: bool,
        promote_prepared_facets: bool,
    ) -> Result<DeferredActorFrameResolutionPlan> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        let plan = deferred_actor_frame_resolution_plan_in_state(
            &mut state,
            resize_epoch,
            promote_tagged,
            promote_prepared_facets,
        );
        drop(state);
        self.cancel_accepted_tokens(plan.cancelled_tokens.iter().copied());
        Ok(plan)
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
        let cancelled_tokens = retire_pending_actor_frames_for_epoch(&mut state, resize_epoch);
        let notifications = ready_actor_frame_notifications(&mut state);
        drop(state);
        self.cancel_accepted_tokens(cancelled_tokens);
        publish_actor_frame_notifications(&self.proxy, notifications);
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
        let cancelled_tokens = retire_pending_actor_frames_for_epoch(&mut state, resize_epoch);
        let notifications = ready_actor_frame_notifications(&mut state);
        drop(state);
        self.cancel_accepted_tokens(cancelled_tokens);
        publish_actor_frame_notifications(&self.proxy, notifications);
        Ok(deferred)
    }

    pub fn diagnostics(&self) -> Result<GraphicsDiagnostics> {
        let state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        let scheduler_metrics;
        let actor_progress;
        let key_to_present_ns;
        let frame_intervals_ns;
        let measured_key_to_present_ns;
        let measured_frame_intervals_ns;
        let warmup_opportunities;
        let measured_opportunities;
        let measured_deadline_misses;
        let measured_target_misses;
        let late_finish_diagnostics;
        let late_finish_diagnostics_dropped;
        let timing_observations;
        {
            let scheduler = self
                .render_scheduler
                .lock()
                .map_err(|_poisoned| anyhow!("render scheduler is poisoned"))?;
            scheduler_metrics = scheduler.metrics();
            actor_progress = scheduler.actor_progress();
            key_to_present_ns = scheduler
                .key_to_present_ns()
                .iter()
                .copied()
                .collect::<Vec<_>>();
            frame_intervals_ns = scheduler
                .presentation_intervals_ns()
                .iter()
                .copied()
                .collect::<Vec<_>>();
            measured_key_to_present_ns = scheduler
                .measured_key_to_present_ns()
                .iter()
                .copied()
                .collect::<Vec<_>>();
            measured_frame_intervals_ns = scheduler
                .measured_presentation_intervals_ns()
                .iter()
                .copied()
                .collect::<Vec<_>>();
            warmup_opportunities = scheduler.warmup_opportunities();
            measured_opportunities = scheduler.measured_opportunities();
            measured_deadline_misses = scheduler.measured_deadline_misses();
            measured_target_misses = scheduler.measured_target_misses();
            late_finish_diagnostics = scheduler
                .late_finish_diagnostics()
                .iter()
                .cloned()
                .collect();
            late_finish_diagnostics_dropped = scheduler.late_finish_diagnostics_dropped();
            timing_observations = scheduler.timing_observations();
        }
        let mut key_samples = measured_key_to_present_ns.clone();
        let mut frame_intervals = measured_frame_intervals_ns.clone();
        key_samples.sort_unstable();
        frame_intervals.sort_unstable();
        let percentile = |samples: &[u64], percent: usize| {
            samples
                .get(samples.len().saturating_sub(1).saturating_mul(percent) / 100)
                .copied()
                .unwrap_or(0)
        };
        let mean = key_samples
            .iter()
            .copied()
            .map(|value| value as f64)
            .sum::<f64>()
            / key_samples.len().max(1) as f64;
        let variance = key_samples
            .iter()
            .copied()
            .map(|value| {
                let distance = value as f64 - mean;
                distance * distance
            })
            .sum::<f64>()
            / key_samples.len().max(1) as f64;
        Ok(GraphicsDiagnostics {
            accepted_packets: state.accepted_packets,
            max_packet_bytes: state.max_packet_bytes,
            max_packet_commands: state.max_packet_commands,
            text_layouts_created: state.text_layouts_created,
            text_layouts_dropped: state.text_layouts_dropped,
            live_text_layouts: state.text_layouts.len(),
            peak_text_layouts: state.peak_text_layouts,
            glyph_raster_backpressure: state.glyph_raster_backpressure,
            render_scheduler: scheduler_metrics,
            key_to_present_p50_ns: percentile(&key_samples, 50),
            key_to_present_p95_ns: percentile(&key_samples, 95),
            key_to_present_p99_ns: percentile(&key_samples, 99),
            key_to_present_max_ns: key_samples.last().copied().unwrap_or(0),
            key_to_present_stddev_ns: variance.sqrt() as u64,
            frame_interval_samples: frame_intervals.len(),
            frame_interval_p95_ns: percentile(&frame_intervals, 95),
            frame_interval_p99_ns: percentile(&frame_intervals, 99),
            key_to_present_ns,
            frame_intervals_ns,
            measured_key_to_present_ns,
            measured_frame_intervals_ns,
            warmup_opportunities,
            measured_opportunities,
            measured_deadline_misses,
            measured_target_misses,
            late_finish_diagnostics,
            late_finish_diagnostics_dropped,
            timing_observations,
            actor_progress,
            actor_frames_accepted: state.actor_frames_accepted,
            actor_frames_ready: state.actor_frames_ready,
            actor_frames_promoted: state.actor_frames_promoted,
            actor_frames_promoted_multi_pane: state.actor_frames_promoted_multi_pane,
            actor_frame_partial_promotion_violations: state
                .actor_frame_partial_promotion_violations,
            visibility_changes: state.visibility_changes,
            max_visible_panes: state.max_visible_panes,
            global_host_tail_estimate_ns: self.host_tail_ns.load(Ordering::Acquire),
            winit_redraw_hop_estimate_ns: self.winit_redraw_hop_ns.load(Ordering::Acquire),
        })
    }
}

fn start_glyph_raster_worker(
    shared: Arc<Mutex<GraphicsState>>,
    text_system: Arc<Mutex<TextSystem>>,
    proxy: EventLoopProxy<ClientEvent>,
) -> SyncSender<GlyphRasterWork> {
    let (sender, receiver) = mpsc::sync_channel::<GlyphRasterWork>(GLYPH_RASTER_QUEUE_CAPACITY);
    let spawn_result = thread::Builder::new()
        .name("brood-glyph-raster".to_owned())
        .spawn(move || {
            while let Ok(work) = receiver.recv() {
                if let GlyphRasterWork::ScaleChanged(scale) = work {
                    rerasterize_text_layouts_for_scale(&shared, &text_system, &proxy, scale);
                    continue;
                }
                let GlyphRasterWork::Layout(request) = work else {
                    continue;
                };
                let started = Instant::now();
                let pane = request.pane;
                let handle = request.handle;
                let result = text_system
                    .lock()
                    .map_err(|_poisoned| PresentationError::HostFailure)
                    .map(|mut text_system| {
                        rasterize_text_layout(
                            &mut text_system,
                            &request.plan,
                            request.options,
                            request.raster_scale,
                        )
                    });
                let elapsed = started.elapsed();
                finish_glyph_raster(&shared, &proxy, &request, result);
                if std::env::var_os("BROOD_TRACE_PLUGIN_TEXT").is_some()
                    || (tracing::enabled!(tracing::Level::INFO) && elapsed.as_millis() >= 8)
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

fn rerasterize_text_layouts_for_scale(
    shared: &Mutex<GraphicsState>,
    text_system: &Mutex<TextSystem>,
    proxy: &EventLoopProxy<ClientEvent>,
    scale: f32,
) {
    let requests = {
        let Ok(state) = shared.lock() else {
            return;
        };
        if state.raster_scale.to_bits() != scale.to_bits() {
            return;
        }
        state
            .text_layouts
            .iter()
            .map(|(handle, entry)| {
                (
                    *handle,
                    entry.pane,
                    entry.generation,
                    Arc::clone(&entry.text),
                    entry.options,
                )
            })
            .collect::<Vec<_>>()
    };
    for (handle, pane, generation, text, options) in requests {
        let request = {
            let Ok(mut text_system) = text_system.lock() else {
                return;
            };
            let Ok(plan) = create_text_layout_plan(&mut text_system.fonts, &text, options) else {
                continue;
            };
            GlyphRasterRequest {
                handle,
                pane,
                generation,
                options,
                plan,
                raster_scale: scale,
            }
        };
        let result = text_system
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)
            .map(|mut text_system| {
                rasterize_text_layout(
                    &mut text_system,
                    &request.plan,
                    request.options,
                    request.raster_scale,
                )
            });
        finish_glyph_raster(shared, proxy, &request, result);
    }
}

fn finish_glyph_raster(
    shared: &Mutex<GraphicsState>,
    proxy: &EventLoopProxy<ClientEvent>,
    request: &GlyphRasterRequest,
    result: Result<TextLayoutResource, PresentationError>,
) {
    let (notifications, actor_frame_notifications) = {
        let Ok(mut state) = shared.lock() else {
            return;
        };
        let Some(entry) = state.text_layouts.get_mut(&request.handle) else {
            return;
        };
        if entry.pane != request.pane || entry.generation != request.generation {
            return;
        }
        if entry.raster_scale.to_bits() != request.raster_scale.to_bits() {
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
        let notifications = state
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
            .collect::<Vec<_>>();
        let actor_frame_notifications = ready_actor_frame_notifications(&mut state);
        (notifications, actor_frame_notifications)
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
    publish_actor_frame_notifications(proxy, actor_frame_notifications);
}

fn packet_references_text_layout(packet: &Packet, handle: u64) -> bool {
    packet.commands.iter().any(
        |command| matches!(command, Command::DrawTextLayout { layout, .. } if *layout == handle),
    )
}

fn release_actor_pane_state(
    state: &mut GraphicsState,
    actor: &str,
    generation: u64,
    panes: &BTreeSet<u64>,
) {
    let scope = ActorScope::new(actor, generation);
    state
        .images
        .retain(|_, resource| !panes.contains(&resource.pane));
    let previous_text_layouts = state.text_layouts.len();
    state
        .text_layouts
        .retain(|_, resource| !panes.contains(&resource.pane));
    state
        .latest_complete_state_watermarks
        .retain(|pane, _| !panes.contains(pane));
    state
        .staged_panes
        .retain(|(pane, _epoch), _scene| !panes.contains(pane));
    state.deferred_panes.retain(|pane, _| !panes.contains(pane));
    state
        .active_resize_epochs
        .retain(|pane, _| !panes.contains(pane));
    let removed = previous_text_layouts.saturating_sub(state.text_layouts.len());
    state.text_layouts_dropped = state
        .text_layouts_dropped
        .saturating_add(u64::try_from(removed).unwrap_or(u64::MAX));
    state.panes.retain(|pane, _| !panes.contains(pane));
    state
        .pending_actor_frames
        .retain(|_revision, frame| frame.actor != scope);
    state
        .deferred_actor_frames
        .retain(|_revision, frame| frame.actor != scope);
    state.actor_panes.remove(&scope);
    state.next_facet_targets.remove(&scope);
}

impl ActorPresentationHost {
    #[must_use]
    pub fn facet_resource_backend(
        self: &Arc<Self>,
        actor_generation: u64,
    ) -> Arc<FacetPresentationBackend> {
        Arc::new(FacetPresentationBackend {
            presentation: Arc::clone(self),
            actor_generation,
        })
    }

    pub fn bind_generation(&self, generation: u64) {
        let previous = self.generation.swap(generation, Ordering::AcqRel);
        self.retired_generation.store(0, Ordering::Release);
        let scope = ActorScope::new(self.actor.clone(), generation);
        if let Ok(mut state) = self.bridge.shared.lock() {
            release_actor_pane_state(&mut state, &self.actor, previous, &self.panes);
            state.actor_panes.insert(scope.clone(), self.panes.clone());
        }
        if let Ok(mut scheduler) = self.bridge.render_scheduler.lock() {
            if previous != 0 {
                scheduler.cancel_generation(&ActorScope::new(self.actor.clone(), previous));
            }
            if scheduler.register(scope.clone()) {
                scheduler.set_relevant_panes(&scope, std::iter::empty());
                scheduler.request_render(&scope);
            }
        }
    }

    /// Revoke scheduler authority as soon as the actor leaves the runtime map.
    /// Native scene retirement remains tied to the atomic pane/layout path.
    pub fn retire_generation(&self, generation: u64) {
        if generation == 0
            || self
                .generation
                .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        self.retired_generation.store(generation, Ordering::Release);
        if let Ok(mut scheduler) = self.bridge.render_scheduler.lock() {
            scheduler.cancel_generation(&ActorScope::new(self.actor.clone(), generation));
        }
    }

    /// Detach the revoked generation after the renderer has retained any CPU
    /// snapshots needed to replay its last accepted layout.
    pub fn detach_retired_state(&self) {
        let generation = self.retired_generation.load(Ordering::Acquire);
        if let Ok(mut state) = self.bridge.shared.lock() {
            release_actor_pane_state(&mut state, &self.actor, generation, &self.panes);
        }
    }

    fn authorize_actor(&self, actor_generation: u64) -> Result<ActorScope, PresentationError> {
        if actor_generation == 0 || actor_generation != self.generation.load(Ordering::Acquire) {
            return Err(PresentationError::ActorStopped);
        }
        Ok(ActorScope::new(self.actor.clone(), actor_generation))
    }

    fn authorize(&self, actor_generation: u64, pane: u64) -> Result<(), PresentationError> {
        if !self.panes.contains(&pane)
            || actor_generation != self.generation.load(Ordering::Acquire)
        {
            return Err(PresentationError::InvalidPane);
        }
        Ok(())
    }

    #[allow(
        dead_code,
        reason = "reached through the not-yet-enabled M2 facet backend"
    )]
    fn create_image_copy_bounded(
        &self,
        actor_generation: u64,
        pane: u64,
        width: u32,
        height: u32,
        pixels: &[u8],
        max_retained_bytes: usize,
    ) -> Result<(u64, usize), PresentationError> {
        self.authorize(actor_generation, pane)?;
        let retained_bytes = image_byte_len(width, height)?;
        if retained_bytes > max_retained_bytes || retained_bytes > MAX_IMAGE_BYTES {
            return Err(PresentationError::LimitReached);
        }
        if retained_bytes != pixels.len() {
            return Err(PresentationError::InvalidArgument);
        }
        // Admission precedes the destination allocation. The caller's borrowed
        // input remains outside this native resource's retained-byte charge.
        let pixels = pixels.to_vec();
        let handle = self.create_image_rgba8(actor_generation, pane, width, height, pixels)?;
        Ok((handle, retained_bytes))
    }

    fn layout_text_bounded(
        &self,
        actor_generation: u64,
        pane: u64,
        text: &str,
        options: TextLayoutOptions,
        max_retained_bytes: usize,
        include_source_text: bool,
    ) -> Result<(TextLayoutResult, usize), PresentationError> {
        self.authorize(actor_generation, pane)?;
        validate_text_request(text, options)?;
        let started = Instant::now();
        // A facet must receive a fresh invocation-scoped handle, but its
        // immutable shaped/raster backing may be shared with an equivalent
        // live resource. Keeping reuse tied to a live entry bounds retention
        // by the existing resource registry and its byte accounting.
        {
            let mut state = self
                .bridge
                .shared
                .lock()
                .map_err(|_poisoned| PresentationError::HostFailure)?;
            if actor_generation != self.generation.load(Ordering::Acquire) {
                return Err(PresentationError::InvalidPane);
            }
            let raster_scale = sanitize_raster_scale(state.raster_scale);
            let reusable = reusable_text_layout(
                &state,
                actor_generation,
                text,
                options,
                raster_scale,
                include_source_text,
            );
            if let Some(reusable) = reusable {
                let ReusableTextLayout {
                    retained_bytes,
                    metrics,
                    text: retained_text,
                    resource,
                } = reusable;
                if retained_bytes > max_retained_bytes {
                    return Err(PresentationError::LimitReached);
                }
                let handle =
                    allocate_resource_handle(&mut state, pane, actor_generation, retained_bytes)?;
                let font_context_revision = state.font_context_revision;
                state.text_layouts.insert(
                    handle,
                    TextLayoutEntry {
                        pane,
                        generation: actor_generation,
                        width: resource.width,
                        height: resource.height,
                        byte_len: retained_bytes,
                        includes_source_text: include_source_text,
                        text: retained_text,
                        options,
                        metrics,
                        font_context_revision,
                        raster_scale,
                        state: TextLayoutState::Ready(resource),
                    },
                );
                state.text_layouts_created = state.text_layouts_created.saturating_add(1);
                state.peak_text_layouts = state.peak_text_layouts.max(state.text_layouts.len());
                return Ok((TextLayoutResult { handle, ..metrics }, retained_bytes));
            }
            drop(state);
        }
        // Shaping scratch is bounded by MAX_TEXT_BYTES, dimensions and
        // MAX_TEXT_LAYOUT_BYTES. It is temporary and is not retained resource
        // backing. The invocation ceiling is checked before retained text,
        // registry state, or raster work is allocated.
        let plan = {
            let mut text_system = self
                .bridge
                .text_system
                .lock()
                .map_err(|_poisoned| PresentationError::HostFailure)?;
            create_text_layout_plan(&mut text_system.fonts, text, options)?
        };
        let metrics = plan.metrics;
        let retained_bytes = if include_source_text {
            plan.byte_len
                .checked_add(text.len())
                .ok_or(PresentationError::LimitReached)?
        } else {
            plan.byte_len
        };
        if retained_bytes > max_retained_bytes {
            return Err(PresentationError::LimitReached);
        }
        let text: Arc<str> = Arc::from(text);
        let mut state = self
            .bridge
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if actor_generation != self.generation.load(Ordering::Acquire) {
            return Err(PresentationError::InvalidPane);
        }
        let raster_scale = sanitize_raster_scale(state.raster_scale);
        let handle = allocate_resource_handle(&mut state, pane, actor_generation, retained_bytes)?;
        let font_context_revision = state.font_context_revision;
        state.text_layouts.insert(
            handle,
            TextLayoutEntry {
                pane,
                generation: actor_generation,
                width: plan.width,
                height: plan.height,
                byte_len: retained_bytes,
                includes_source_text: include_source_text,
                text,
                options,
                metrics,
                font_context_revision,
                raster_scale,
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
            raster_scale,
        };
        if self
            .bridge
            .glyph_raster_tx
            .try_send(GlyphRasterWork::Layout(Box::new(request)))
            .is_err()
        {
            if let Ok(mut state) = self.bridge.shared.lock() {
                state.text_layouts.remove(&handle);
                state.glyph_raster_backpressure = state.glyph_raster_backpressure.saturating_add(1);
            }
            return Err(PresentationError::LimitReached);
        }
        let elapsed = started.elapsed();
        if std::env::var_os("BROOD_TRACE_PLUGIN_TEXT").is_some()
            || (tracing::enabled!(tracing::Level::INFO) && elapsed.as_millis() >= 8)
        {
            eprintln!(
                "plugin text layout pane={pane} handle={handle} bytes={} time_us={}",
                retained_bytes,
                elapsed.as_micros(),
            );
        }
        Ok((TextLayoutResult { handle, ..metrics }, retained_bytes))
    }
}

impl FacetResourceBackend for FacetPresentationBackend {
    fn create_image(
        &self,
        pane: u64,
        width: u32,
        height: u32,
        pixels: &[u8],
        max_bytes: usize,
    ) -> Result<(u64, usize), PresentationError> {
        self.presentation.create_image_copy_bounded(
            self.actor_generation,
            pane,
            width,
            height,
            pixels,
            max_bytes,
        )
    }

    fn create_text(
        &self,
        pane: u64,
        text: &str,
        options: TextLayoutOptions,
        max_bytes: usize,
    ) -> Result<(u64, TextLayoutResult, usize), PresentationError> {
        let (result, retained_bytes) = self.presentation.layout_text_bounded(
            self.actor_generation,
            pane,
            text,
            options,
            max_bytes,
            true,
        )?;
        Ok((result.handle, result, retained_bytes))
    }

    fn release(&self, pane: u64, handle: u64) -> Result<(), PresentationError> {
        self.presentation
            .drop_resource(self.actor_generation, pane, handle)
    }
}

impl Drop for ActorPresentationHost {
    fn drop(&mut self) {
        let generation = self.generation.load(Ordering::Acquire);
        let cleanup_generation = if generation == 0 {
            self.retired_generation.load(Ordering::Acquire)
        } else {
            generation
        };
        if let Ok(mut state) = self.bridge.shared.lock() {
            release_actor_pane_state(&mut state, &self.actor, cleanup_generation, &self.panes);
        }
        if generation != 0
            && let Ok(mut scheduler) = self.bridge.render_scheduler.lock()
        {
            scheduler.cancel_generation(&ActorScope::new(self.actor.clone(), generation));
        }
    }
}

impl ActorPresentationHost {
    pub fn require_facet_input_reduction(&self, actor_generation: u64) -> bool {
        self.bridge
            .render_scheduler
            .lock()
            .is_ok_and(|mut scheduler| {
                scheduler.require_input_correlation_reduction(&ActorScope::new(
                    self.actor.clone(),
                    actor_generation,
                ))
            })
    }

    pub fn submit_facet_frame(
        &self,
        actor_generation: u64,
        scheduler_token: u64,
        receipt: PreparedFacetRender,
        resize_epochs: &BTreeMap<u64, Option<u64>>,
        budget: &ByteBudget,
    ) -> Result<u64, PresentationError> {
        let guest_finished_ns = self.bridge.render_now_ns();
        let validation_started = Instant::now();
        let scope = self.authorize_actor(actor_generation)?;
        receipt
            .validate_current()
            .map_err(|_error| PresentationError::InvalidToken)?;
        let decoded = receipt
            .with_graphics(|graphics| {
                let facet_panes = graphics
                    .panes()
                    .map(|(pane, _frame)| *pane)
                    .collect::<BTreeSet<_>>();
                if facet_panes != self.panes
                    || resize_epochs.keys().copied().collect::<BTreeSet<_>>() != self.panes
                {
                    return Err(PresentationError::InvalidPane);
                }
                graphics
                    .panes()
                    .map(|(pane, frame)| {
                        self.authorize(actor_generation, *pane)?;
                        let resize_epoch = resize_epochs
                            .get(pane)
                            .copied()
                            .ok_or(PresentationError::InvalidPane)?;
                        if resize_epoch == Some(0) {
                            return Err(PresentationError::StaleResizeEpoch);
                        }
                        let packet = SharedPacket::decode_facet(
                            frame
                                .packet()
                                .map_err(|_error| PresentationError::InvalidPacket)?,
                            budget,
                            actor_generation,
                        )?;
                        Ok((*pane, resize_epoch, Arc::clone(frame), packet))
                    })
                    .collect::<Result<Vec<_>, PresentationError>>()
            })
            .map_err(|_error| PresentationError::InvalidToken)??;
        let panes = decoded
            .iter()
            .map(|(pane, ..)| *pane)
            .collect::<BTreeSet<_>>();
        let mut state = self
            .bridge
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if state.pending_actor_frames.len() >= MAX_RENDER_ACTORS
            && !state
                .pending_actor_frames
                .values()
                .any(|frame| frame.actor == scope)
        {
            return Err(PresentationError::LimitReached);
        }
        for (pane, resize_epoch, frame, packet) in &decoded {
            if let Some(epoch) = resize_epoch
                && !resize_epoch_is_active(&state, *pane, *epoch)
            {
                return Err(PresentationError::StaleResizeEpoch);
            }
            let resources = referenced_resources(&state, *pane, actor_generation, packet)?;
            let candidate = GraphicsSnapshot {
                facet_extent: Some((
                    frame.geometry().physical_width,
                    frame.geometry().physical_height,
                )),
                pane: *pane,
                sequence: 0,
                resize_epoch: *resize_epoch,
                packet: packet.clone(),
                images: resources.images,
                text_layouts: resources.text_layouts,
                glyphs: resources.glyphs,
                facet_frame_lease: Some(Arc::clone(frame)),
            };
            crate::plugin_gpu::validate_snapshot_lowering(&candidate)?;
        }
        let next_sequence = state
            .next_sequence
            .checked_add(1)
            .ok_or(PresentationError::LimitReached)?;
        let accepted_ns = self.bridge.render_now_ns();
        let mut scheduler = self
            .bridge
            .render_scheduler
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        let accepted = scheduler
            .finish_for_actor_timed(
                &scope,
                RenderToken(scheduler_token),
                guest_finished_ns,
                accepted_ns,
                &panes.iter().copied().collect::<Vec<_>>(),
            )
            .map_err(|_error| PresentationError::InvalidToken)?;
        scheduler.note_actor_host_cost(
            &scope,
            u64::try_from(validation_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
        drop(scheduler);
        let Some(accepted) = accepted else {
            return Ok(0);
        };
        state.next_sequence = next_sequence;
        state
            .pending_actor_frames
            .retain(|_revision, frame| frame.actor != scope);
        let scenes = decoded
            .into_iter()
            .map(|(pane, resize_epoch, frame, packet)| {
                state.accepted_packets = state.accepted_packets.saturating_add(1);
                state.max_packet_commands = state.max_packet_commands.max(packet.commands.len());
                (
                    pane,
                    PaneScene {
                        generation: actor_generation,
                        sequence: next_sequence,
                        resize_epoch,
                        terminal_input_sequence: None,
                        packet,
                        facet_frame: Some(frame),
                    },
                )
            })
            .collect();
        state.pending_actor_frames.insert(
            accepted.actor_frame_revision,
            PendingGraphicsActorFrame {
                actor: scope,
                token: scheduler_token,
                revision: accepted.actor_frame_revision,
                scenes,
                ready_notified: false,
                required_panes: Some(panes),
                blocker_epochs: BTreeSet::new(),
                facet_receipt: Some(receipt),
                facet_atomic: true,
            },
        );
        state.actor_frames_accepted = state.actor_frames_accepted.saturating_add(1);
        if std::env::var_os("BROOD_TRACE_PLUGIN_GRAPHICS").is_some() {
            eprintln!(
                "facet bridge accepted token={scheduler_token} revision={} visible={:?}",
                accepted.actor_frame_revision, state.visible_panes
            );
        }
        let notifications = ready_actor_frame_notifications(&mut state);
        drop(state);
        publish_actor_frame_notifications(&self.bridge.proxy, notifications);
        self.bridge
            .proxy
            .send_event(ClientEvent::RenderSchedulingChanged)
            .map_err(|_closed| PresentationError::ActorStopped)?;
        Ok(accepted.actor_frame_revision)
    }
}

impl PresentationHost for ActorPresentationHost {
    fn request_render(&self, actor_generation: u64) -> Result<(), PresentationError> {
        let scope = self.authorize_actor(actor_generation)?;
        let mut scheduler = self
            .bridge
            .render_scheduler
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if !scheduler.request_render(&scope) {
            return Err(PresentationError::ActorStopped);
        }
        drop(scheduler);
        self.bridge
            .proxy
            .send_event(ClientEvent::RenderSchedulingChanged)
            .map_err(|_closed| PresentationError::ActorStopped)
    }

    fn set_render_subscription(
        &self,
        actor_generation: u64,
        enabled: bool,
    ) -> Result<(), PresentationError> {
        let scope = self.authorize_actor(actor_generation)?;
        let mut scheduler = self
            .bridge
            .render_scheduler
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if !scheduler.set_subscription(&scope, enabled) {
            return Err(PresentationError::ActorStopped);
        }
        drop(scheduler);
        self.bridge
            .proxy
            .send_event(ClientEvent::RenderSchedulingChanged)
            .map_err(|_closed| PresentationError::ActorStopped)
    }

    fn finish_actor_frame(
        &self,
        actor_generation: u64,
        token: GuestRenderToken,
        draws: Vec<ActorFrameDraw>,
    ) -> Result<u64, PresentationError> {
        let guest_finished_ns = self.bridge.render_now_ns();
        let validation_started = Instant::now();
        let scope = self.authorize_actor(actor_generation)?;
        if draws.len() > MAX_RELEVANT_PANES {
            return Err(PresentationError::LimitReached);
        }
        let mut panes = BTreeSet::new();
        let mut state = self
            .bridge
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if state.pending_actor_frames.len() >= MAX_RENDER_ACTORS
            && !state
                .pending_actor_frames
                .values()
                .any(|frame| frame.actor == scope)
        {
            return Err(PresentationError::LimitReached);
        }
        // Validate the complete batch's identity and causal bounds before
        // decoding packets, resolving resources, or consuming the render
        // token. One invalid pane therefore rejects the batch transactionally.
        let decoded = validate_actor_frame_before_preparation(
            draws,
            |draw| {
                self.authorize(actor_generation, draw.pane)?;
                if !panes.insert(draw.pane) {
                    return Err(PresentationError::DuplicatePane);
                }
                if draw.resize_epoch == Some(0) {
                    return Err(PresentationError::StaleResizeEpoch);
                }
                validate_state_watermark(&state, draw.pane, draw.state_watermark)?;
                if let Some(epoch) = draw.resize_epoch
                    && !resize_epoch_is_active(&state, draw.pane, epoch)
                {
                    return Err(PresentationError::StaleResizeEpoch);
                }
                Ok(())
            },
            |draw| {
                let packet = SharedPacket::from(
                    Packet::decode(&draw.packet).map_err(packet_error_to_presentation)?,
                );
                let resources = referenced_resources(&state, draw.pane, actor_generation, &packet)?;
                let candidate = GraphicsSnapshot {
                    facet_extent: None,
                    pane: draw.pane,
                    sequence: 0,
                    resize_epoch: draw.resize_epoch,
                    packet,
                    images: resources.images,
                    text_layouts: resources.text_layouts,
                    glyphs: resources.glyphs,
                    facet_frame_lease: None,
                };
                crate::plugin_gpu::validate_snapshot_lowering(&candidate)?;
                Ok((
                    draw.pane,
                    draw.resize_epoch,
                    draw.state_watermark,
                    draw.packet.len(),
                    candidate.packet,
                ))
            },
        )?;
        let next_sequence = state
            .next_sequence
            .checked_add(1)
            .ok_or(PresentationError::LimitReached)?;
        // This is the exact serial-stage boundary: guest validation ends here
        // and accepted-to-graphics-ready readiness begins here. Sampling the
        // host cost after scheduler acceptance would count that acceptance in
        // both estimates.
        let actor_host_cost_ns =
            u64::try_from(validation_started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let accepted_ns = self.bridge.render_now_ns();
        let mut scheduler = self
            .bridge
            .render_scheduler
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        let accepted = scheduler
            .finish_for_actor_timed(
                &scope,
                RenderToken(token.0),
                guest_finished_ns,
                accepted_ns,
                &panes.iter().copied().collect::<Vec<_>>(),
            )
            .map_err(|_error| PresentationError::InvalidToken)?;
        scheduler.note_actor_host_cost(&scope, actor_host_cost_ns);
        drop(scheduler);
        let Some(accepted) = accepted else {
            drop(state);
            self.bridge
                .proxy
                .send_event(ClientEvent::RenderSchedulingChanged)
                .map_err(|_closed| PresentationError::ActorStopped)?;
            return Ok(0);
        };
        if std::env::var_os("BROOD_TRACE_RESIZE_PIPELINE").is_some()
            && matches!(scope.name.as_str(), "session-switcher" | "terminal-ui")
        {
            let pane_watermarks = decoded
                .iter()
                .map(|(pane, _epoch, watermark, _bytes, _packet)| {
                    format!("{pane}:{}", watermark.unwrap_or(0))
                })
                .collect::<Vec<_>>()
                .join(",");
            eprintln!(
                "resize-pipeline-frame token={} actor={} generation={} sequence={} guest_finished_ns={} accepted_ns={} validation_ns={} pane_watermarks=[{}]",
                token.0,
                scope.name,
                scope.generation,
                next_sequence,
                guest_finished_ns,
                accepted_ns,
                actor_host_cost_ns,
                pane_watermarks,
            );
        }
        state.next_sequence = next_sequence;
        state
            .pending_actor_frames
            .retain(|_revision, frame| frame.actor != scope);
        let mut scenes = BTreeMap::new();
        for (pane, resize_epoch, state_watermark, packet_bytes, packet) in decoded {
            state.accepted_packets = state.accepted_packets.saturating_add(1);
            state.max_packet_bytes = state.max_packet_bytes.max(packet_bytes);
            state.max_packet_commands = state.max_packet_commands.max(packet.commands.len());
            scenes.insert(
                pane,
                PaneScene {
                    generation: actor_generation,
                    sequence: next_sequence,
                    resize_epoch,
                    terminal_input_sequence: state_watermark,
                    packet,
                    facet_frame: None,
                },
            );
        }
        state.pending_actor_frames.insert(
            accepted.actor_frame_revision,
            PendingGraphicsActorFrame {
                actor: scope,
                token: token.0,
                revision: accepted.actor_frame_revision,
                scenes,
                ready_notified: false,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
                facet_receipt: None,
                facet_atomic: false,
            },
        );
        state.actor_frames_accepted = state.actor_frames_accepted.saturating_add(1);
        let notifications = ready_actor_frame_notifications(&mut state);
        drop(state);
        publish_actor_frame_notifications(&self.bridge.proxy, notifications);
        self.bridge
            .proxy
            .send_event(ClientEvent::RenderSchedulingChanged)
            .map_err(|_closed| PresentationError::ActorStopped)?;
        Ok(accepted.actor_frame_revision)
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
        self.layout_text_bounded(actor_generation, pane, &text, options, usize::MAX, false)
            .map(|(result, _retained_bytes)| result)
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
        if resource_is_referenced(&state, resource) {
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
                            raster_scale: entry.raster_scale,
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

#[allow(
    dead_code,
    reason = "reached through the not-yet-enabled M2 facet backend"
)]
fn image_byte_len(width: u32, height: u32) -> Result<usize, PresentationError> {
    if width == 0 || height == 0 {
        return Err(PresentationError::InvalidArgument);
    }
    if width > MAX_RESOURCE_DIMENSION || height > MAX_RESOURCE_DIMENSION {
        return Err(PresentationError::LimitReached);
    }
    usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(PresentationError::InvalidArgument)
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

fn sanitize_raster_scale(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale.clamp(0.5, 8.0)
    } else {
        DEFAULT_RASTER_SCALE
    }
}

fn rasterize_text_layout(
    text_system: &mut TextSystem,
    plan: &TextLayoutPlan,
    options: TextLayoutOptions,
    raster_scale: f32,
) -> TextLayoutResource {
    let raster_scale = sanitize_raster_scale(raster_scale);
    let requested_color = options.color.map(unit_to_u8);
    let positioned = plan
        .buffer
        .layout_runs()
        .flat_map(|run| {
            run.glyphs.iter().map(move |glyph| {
                // Cosmic's offset is already in device pixels; glyph positions
                // and size are scaled by physical(), but the line origin is not.
                let physical = glyph.physical((0.0, run.line_y * raster_scale), raster_scale);
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
        let image = if let Some(cached) = text_system.glyphs.get(&key) {
            cached.clone()
        } else {
            // SwashCache's built-in image map has no eviction policy. Render
            // uncached and retain only in Brood's explicitly bounded cache.
            let rendered = text_system
                .swash
                .get_image_uncached(&mut text_system.fonts, cache_key)
                .as_ref()
                .and_then(|image| glyph_image_resource(image, requested_color));
            cache_glyph_image(text_system, key, rendered.clone());
            rendered
        };
        let Some(image) = image else {
            continue;
        };
        sprites.push(GlyphSprite {
            key,
            x: x.saturating_add(image.left) as f32 / raster_scale,
            y: y.saturating_sub(image.top) as f32 / raster_scale,
        });
        glyph_images.insert(key, image);
    }
    TextLayoutResource {
        width: plan.width,
        height: plan.height,
        glyphs: sprites.into(),
        glyph_images,
        raster_scale,
    }
}

fn cache_glyph_image(
    text_system: &mut TextSystem,
    key: GlyphCacheKey,
    image: Option<GlyphImageResource>,
) {
    let image_bytes = image
        .as_ref()
        .map_or(0, |image| image.linear_premultiplied_rgba8.len());
    if image_bytes > MAX_GLYPH_CACHE_BYTES {
        return;
    }
    while text_system.glyphs.len() >= MAX_GLYPH_CACHE_ENTRIES
        || text_system
            .glyph_cache_bytes
            .checked_add(image_bytes)
            .is_none_or(|bytes| bytes > MAX_GLYPH_CACHE_BYTES)
    {
        let Some(evicted_key) = text_system.glyphs.first_key_value().map(|(key, _)| *key) else {
            break;
        };
        if let Some(Some(evicted)) = text_system.glyphs.remove(&evicted_key) {
            text_system.glyph_cache_bytes = text_system
                .glyph_cache_bytes
                .saturating_sub(evicted.linear_premultiplied_rgba8.len());
        }
    }
    text_system.glyph_cache_bytes = text_system.glyph_cache_bytes.saturating_add(image_bytes);
    text_system.glyphs.insert(key, image);
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
#[allow(
    clippy::unwrap_used,
    reason = "graphics contract tests fail immediately when required staged state is absent"
)]
mod tests {
    use super::*;

    #[test]
    fn facet_targets_admit_replace_cancel_and_prearm_once() {
        let mut state = GraphicsState::default();
        let actor = ActorScope::new("animated", 7);
        state.actor_panes.insert(actor.clone(), BTreeSet::from([1]));
        state.visible_panes.insert(1);

        replace_facet_target(&mut state, &actor, Some(100));
        assert_eq!(state.next_facet_targets.get(&actor), Some(&100));
        replace_facet_target(&mut state, &actor, Some(250));
        assert_eq!(take_facet_targets(&mut state), vec![(actor.clone(), 250)]);
        assert_eq!(take_facet_targets(&mut state), Vec::new());

        replace_facet_target(&mut state, &actor, Some(300));
        replace_facet_target(&mut state, &actor, None);
        assert!(state.next_facet_targets.is_empty());
    }

    #[test]
    fn hidden_facet_deadline_is_retired_without_requesting_a_render() {
        let mut state = GraphicsState::default();
        let actor = ActorScope::new("hidden", 7);
        state.actor_panes.insert(actor.clone(), BTreeSet::from([1]));
        replace_facet_target(&mut state, &actor, Some(100));

        assert_eq!(take_facet_targets(&mut state), Vec::new());
        assert!(state.next_facet_targets.is_empty());
    }

    #[test]
    fn facet_targets_are_generation_scoped_and_cleared_on_actor_replacement() {
        let actor = "animated";
        let old = ActorScope::new(actor, 7);
        let replacement = ActorScope::new(actor, 8);
        let unrelated = ActorScope::new("other", 7);
        let mut state = GraphicsState::default();
        replace_facet_target(&mut state, &old, Some(100));
        replace_facet_target(&mut state, &replacement, Some(200));
        replace_facet_target(&mut state, &unrelated, Some(300));

        release_actor_pane_state(&mut state, actor, 7, &BTreeSet::new());

        assert_eq!(state.next_facet_targets.len(), 2);
        assert_eq!(state.next_facet_targets.get(&replacement), Some(&200));
        assert_eq!(state.next_facet_targets.get(&unrelated), Some(&300));
    }

    #[test]
    fn shared_facet_packet_keeps_its_budget_charge_until_the_last_clone() -> Result<()> {
        let bytes = Packet::default().encode()?;
        let retained = Packet::decode_allocation_bound(&bytes)? + SHARED_PACKET_METADATA_BYTES;
        let budget = ByteBudget::new(
            7,
            brood_wasm::snapshot_budget::ByteBudgetLimits {
                max_live_bytes: retained,
                max_live_allocations: 1,
                max_buffer_bytes: retained,
            },
        );
        let packet = SharedPacket::decode_facet(&bytes, &budget, 7)
            .map_err(|error| anyhow!(error.to_string()))?;
        anyhow::ensure!(budget.retention().live_bytes == retained);
        anyhow::ensure!(budget.retention().live_allocations == 1);

        let retained_packet = packet.clone();
        drop(packet);
        anyhow::ensure!(budget.retention().live_bytes == retained);
        drop(retained_packet);
        anyhow::ensure!(budget.retention().live_bytes == 0);
        anyhow::ensure!(budget.retention().live_allocations == 0);
        Ok(())
    }

    #[test]
    fn shared_facet_packet_rejects_a_foreign_generation_before_admission() -> Result<()> {
        let bytes = Packet::default().encode()?;
        let budget = ByteBudget::new(
            8,
            brood_wasm::snapshot_budget::ByteBudgetLimits {
                max_live_bytes: 1_024,
                max_live_allocations: 1,
                max_buffer_bytes: 1_024,
            },
        );
        anyhow::ensure!(matches!(
            SharedPacket::decode_facet(&bytes, &budget, 7),
            Err(PresentationError::ActorStopped)
        ));
        anyhow::ensure!(budget.retention().live_bytes == 0);
        Ok(())
    }

    fn test_text_options() -> TextLayoutOptions {
        TextLayoutOptions {
            font_intent: FontIntent::Ui,
            font_weight: FontWeight::Normal,
            font_style: FontStyle::Normal,
            font_size: 16.0,
            max_width: 256.0,
            line_height: 20.0,
            color: [1.0; 4],
        }
    }

    #[test]
    fn glyph_rasterization_tracks_fractional_device_scale() -> Result<()> {
        let mut text_system = TextSystem {
            fonts: FontSystem::new(),
            swash: SwashCache::new(),
            glyphs: BTreeMap::new(),
            glyph_cache_bytes: 0,
        };
        let options = test_text_options();
        let plan = create_text_layout_plan(&mut text_system.fonts, "Brood", options)?;
        let one_x = rasterize_text_layout(&mut text_system, &plan, options, 1.0);
        let plan = create_text_layout_plan(&mut text_system.fonts, "Brood", options)?;
        let fractional = rasterize_text_layout(&mut text_system, &plan, options, 1.5);
        let raster_area = |resource: &TextLayoutResource| {
            resource
                .glyph_images
                .values()
                .map(|glyph| u64::from(glyph.width) * u64::from(glyph.height))
                .sum::<u64>()
        };

        anyhow::ensure!(one_x.raster_scale.to_bits() == 1.0_f32.to_bits());
        anyhow::ensure!(fractional.raster_scale.to_bits() == 1.5_f32.to_bits());
        anyhow::ensure!(raster_area(&one_x) > 0);
        anyhow::ensure!(raster_area(&fractional) > raster_area(&one_x));
        Ok(())
    }

    #[test]
    fn fractional_rasterization_preserves_logical_baselines_and_line_spacing() -> Result<()> {
        let mut system = TextSystem {
            fonts: FontSystem::new(),
            swash: SwashCache::new(),
            glyphs: BTreeMap::new(),
            glyph_cache_bytes: 0,
        };
        let options = test_text_options();
        let plan = create_text_layout_plan(&mut system.fonts, "B\nB", options)?;
        let reference = rasterize_text_layout(&mut system, &plan, options, 1.0);
        anyhow::ensure!(reference.glyphs.len() == 2);
        for scale in [1.25, 1.5, 1.75, 2.0] {
            let scaled = rasterize_text_layout(&mut system, &plan, options, scale);
            anyhow::ensure!(scaled.glyphs.len() == 2);
            for (actual, expected) in scaled.glyphs.iter().zip(reference.glyphs.iter()) {
                anyhow::ensure!(
                    (actual.y - expected.y).abs() <= 1.5,
                    "glyph moved vertically at scale {scale}: {} vs {}",
                    actual.y,
                    expected.y
                );
            }
            let spacing = scaled.glyphs.get(1).context("second glyph missing")?.y
                - scaled.glyphs.first().context("first glyph missing")?.y;
            anyhow::ensure!((spacing - options.line_height).abs() <= 1.0);
        }
        Ok(())
    }

    #[test]
    fn native_glyph_cache_has_a_hard_entry_bound() -> Result<()> {
        let mut text_system = TextSystem {
            fonts: FontSystem::new(),
            swash: SwashCache::new(),
            glyphs: BTreeMap::new(),
            glyph_cache_bytes: 0,
        };
        let cache_key = create_text_layout_plan(&mut text_system.fonts, "a", test_text_options())?
            .buffer
            .layout_runs()
            .next()
            .and_then(|run| {
                run.glyphs
                    .first()
                    .map(|glyph| glyph.physical((0.0, run.line_y), 1.0).cache_key)
            })
            .context("test font produced no glyph")?;
        for color in 0..=MAX_GLYPH_CACHE_ENTRIES {
            let [low, high] = u16::try_from(color).unwrap_or(u16::MAX).to_le_bytes();
            cache_glyph_image(
                &mut text_system,
                GlyphCacheKey {
                    cache_key,
                    color: [low, high, 0, 255],
                },
                None,
            );
        }
        anyhow::ensure!(text_system.glyphs.len() == MAX_GLYPH_CACHE_ENTRIES);
        anyhow::ensure!(text_system.glyph_cache_bytes == 0);
        Ok(())
    }

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
            includes_source_text: false,
            text: Arc::from("x"),
            options: test_text_options(),
            metrics: TextLayoutResult {
                handle: 0,
                width: 1.0,
                height: 1.0,
                first_baseline: 0.0,
                line_count: 1,
                cell_width: 0.0,
                line_height: 1.0,
            },
            font_context_revision: 0,
            raster_scale: 1.0,
            state: TextLayoutState::Pending,
        }
    }

    fn reusable_ready_text_entry(
        text: &str,
        options: TextLayoutOptions,
        raster_scale: f32,
        byte_len: usize,
    ) -> TextLayoutEntry {
        let mut entry = pending_text_entry();
        entry.text = Arc::from(text);
        entry.options = options;
        entry.raster_scale = raster_scale;
        entry.byte_len = byte_len;
        entry.includes_source_text = true;
        entry.metrics.line_height = options.line_height;
        entry.state = TextLayoutState::Ready(TextLayoutResource {
            raster_scale,
            ..ready_text_layout()
        });
        entry
    }

    #[test]
    fn live_text_backing_reuse_requires_complete_shape_key() {
        let options = test_text_options();
        let mut state = GraphicsState::default();
        state.text_layouts.insert(
            1,
            reusable_ready_text_entry("multilingual العربية 日本語", options, 1.5, 4096),
        );

        let hit = reusable_text_layout(
            &state,
            11,
            "multilingual العربية 日本語",
            options,
            1.5,
            true,
        );
        assert!(hit.is_some());

        let mut different_width = options;
        different_width.max_width += 1.0;
        assert!(
            reusable_text_layout(
                &state,
                11,
                "multilingual العربية 日本語",
                different_width,
                1.5,
                true,
            )
            .is_none()
        );
        assert!(
            reusable_text_layout(
                &state,
                11,
                "multilingual العربية 日本語",
                options,
                2.0,
                true,
            )
            .is_none()
        );
        assert!(
            reusable_text_layout(
                &state,
                12,
                "multilingual العربية 日本語",
                options,
                1.5,
                true,
            )
            .is_none()
        );
        assert!(
            reusable_text_layout(
                &state,
                11,
                "multilingual العربية 日本語",
                options,
                1.5,
                false,
            )
            .is_none()
        );
        state.font_context_revision = 1;
        assert!(
            reusable_text_layout(
                &state,
                11,
                "multilingual العربية 日本語",
                options,
                1.5,
                true,
            )
            .is_none()
        );
    }

    #[test]
    fn reused_text_backing_still_allocates_and_charges_a_fresh_handle() -> Result<()> {
        let options = test_text_options();
        let mut state = GraphicsState {
            next_resource: 40,
            ..GraphicsState::default()
        };
        state
            .text_layouts
            .insert(40, reusable_ready_text_entry("same", options, 1.0, 4096));
        let reused = reusable_text_layout(&state, 11, "same", options, 1.0, true)
            .ok_or_else(|| anyhow!("live backing was not reusable"))?;
        let handle = allocate_resource_handle(&mut state, 7, 11, reused.retained_bytes)?;
        anyhow::ensure!(handle == 41 && handle != 40, "handle was reused");
        let font_context_revision = state.font_context_revision;
        state.text_layouts.insert(
            handle,
            TextLayoutEntry {
                pane: 7,
                generation: 11,
                width: reused.resource.width,
                height: reused.resource.height,
                byte_len: reused.retained_bytes,
                includes_source_text: true,
                text: reused.text,
                options,
                metrics: reused.metrics,
                font_context_revision,
                raster_scale: 1.0,
                state: TextLayoutState::Ready(reused.resource),
            },
        );
        let charged = state
            .text_layouts
            .values()
            .filter(|entry| entry.pane == 7 && entry.generation == 11)
            .map(|entry| entry.byte_len)
            .sum::<usize>();
        anyhow::ensure!(
            charged == 8192,
            "fresh handle was not charged independently"
        );
        anyhow::ensure!(
            allocate_resource_handle(
                &mut state,
                7,
                11,
                MAX_RESOURCE_BYTES_PER_PANE
                    .saturating_sub(charged)
                    .saturating_add(1),
            ) == Err(PresentationError::LimitReached),
            "reused backing bypassed the pane byte ceiling"
        );
        Ok(())
    }

    #[test]
    fn text_backing_reuse_ends_with_the_last_live_resource() {
        let options = test_text_options();
        let mut state = GraphicsState::default();
        state
            .text_layouts
            .insert(1, reusable_ready_text_entry("bounded", options, 1.0, 128));
        assert!(reusable_text_layout(&state, 11, "bounded", options, 1.0, true).is_some());

        state.text_layouts.remove(&1);
        assert!(reusable_text_layout(&state, 11, "bounded", options, 1.0, true).is_none());
    }

    #[test]
    fn text_backing_reuse_scan_is_bounded_and_old_match_is_a_clean_miss() -> Result<()> {
        let options = test_text_options();
        let mut state = GraphicsState::default();
        state.text_layouts.insert(
            1,
            reusable_ready_text_entry("old matching layout", options, 1.0, 128),
        );
        for offset in 0..MAX_TEXT_BACKING_REUSE_SCAN {
            let handle = u64::try_from(offset)?.saturating_add(2);
            state.text_layouts.insert(
                handle,
                reusable_ready_text_entry(&format!("recent layout {handle}"), options, 1.0, 128),
            );
        }
        state.next_resource = 100;
        let entries_before = state.text_layouts.len();
        let bytes_before = state
            .text_layouts
            .values()
            .map(|entry| entry.byte_len)
            .sum::<usize>();

        anyhow::ensure!(
            reusable_text_layout(&state, 11, "old matching layout", options, 1.0, true).is_none(),
            "an entry outside the bounded scan window was reused"
        );
        anyhow::ensure!(state.text_layouts.len() == entries_before);
        anyhow::ensure!(
            state
                .text_layouts
                .values()
                .map(|entry| entry.byte_len)
                .sum::<usize>()
                == bytes_before
        );
        anyhow::ensure!(state.next_resource == 100);
        Ok(())
    }

    fn ready_text_layout() -> TextLayoutResource {
        TextLayoutResource {
            width: 1,
            height: 1,
            glyphs: Arc::from([]),
            glyph_images: BTreeMap::new(),
            raster_scale: 1.0,
        }
    }

    fn resize_test_scene(sequence: u64, resize_epoch: Option<u64>) -> PaneScene {
        PaneScene {
            facet_frame: None,
            generation: 2,
            sequence,
            resize_epoch,
            terminal_input_sequence: Some(sequence),
            packet: Packet::default().into(),
        }
    }

    fn insert_resize_test_frame(
        state: &mut GraphicsState,
        revision: u64,
        token: u64,
        scenes: impl IntoIterator<Item = (u64, PaneScene)>,
    ) {
        state.pending_actor_frames.insert(
            revision,
            PendingGraphicsActorFrame {
                actor: ActorScope::new(format!("resize-{revision}"), 2),
                token,
                revision,
                scenes: scenes.into_iter().collect(),
                ready_notified: true,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
                facet_receipt: None,
                facet_atomic: false,
            },
        );
    }

    #[test]
    fn accepted_actor_frames_keep_referenced_resources_live() {
        let scene = |resource| PaneScene {
            generation: 11,
            sequence: resource,
            resize_epoch: None,
            terminal_input_sequence: None,
            packet: text_packet(&[resource]).into(),
            facet_frame: None,
        };
        let frame = |revision, resource| PendingGraphicsActorFrame {
            actor: ActorScope::new("resource-owner", 11),
            token: revision,
            revision,
            scenes: BTreeMap::from([(7, scene(resource))]),
            ready_notified: false,
            required_panes: None,
            blocker_epochs: BTreeSet::new(),
            facet_receipt: None,
            facet_atomic: false,
        };
        let mut state = GraphicsState::default();
        state.pending_actor_frames.insert(1, frame(1, 41));
        state.deferred_actor_frames.insert(2, frame(2, 42));

        assert!(resource_is_referenced(&state, 41));
        assert!(resource_is_referenced(&state, 42));
        assert!(!resource_is_referenced(&state, 43));
    }

    fn visible_resize_sequences(state: &GraphicsState) -> Vec<(u64, u64)> {
        state
            .panes
            .iter()
            .map(|(pane, scene)| (*pane, scene.sequence))
            .collect()
    }

    fn defer_resize_test_frame(
        state: &mut GraphicsState,
        revision: u64,
        panes: &[u64],
    ) -> Option<(u64, BTreeSet<u64>, Vec<u64>)> {
        defer_prepared_actor_frame_in_state(state, revision, panes)
            .unwrap()
            .map(|stage| {
                (
                    stage.token,
                    stage.blocker_epochs,
                    stage.superseded_revisions,
                )
            })
    }

    #[test]
    fn active_resize_actor_frames_stage_complete_tagged_epoch_without_ordinary_leaks() {
        let mut state = GraphicsState::default();
        state.panes.insert(7, resize_test_scene(5, None));
        state.panes.insert(8, resize_test_scene(5, None));
        state.visible_panes.extend([7, 8]);
        state.active_resize_epochs.extend([(7, 6), (8, 6)]);

        insert_resize_test_frame(&mut state, 10, 100, [(7, resize_test_scene(6, None))]);
        assert_eq!(
            claim_actor_frame_for_preparation_in_state(&mut state, 10, &[7]).unwrap(),
            ActorFramePreparationClaim::PrepareDeferred
        );
        assert_eq!(
            defer_resize_test_frame(&mut state, 10, &[7]),
            Some((100, BTreeSet::from([6]), Vec::new()))
        );
        assert_eq!(visible_resize_sequences(&state), [(7, 5), (8, 5)]);
        assert!(
            actor_frame_snapshots_from_state(&state, 10, [7])
                .unwrap()
                .is_none()
        );

        insert_resize_test_frame(&mut state, 11, 101, [(8, resize_test_scene(7, None))]);
        assert_eq!(
            claim_actor_frame_for_preparation_in_state(&mut state, 11, &[8]).unwrap(),
            ActorFramePreparationClaim::PrepareDeferred
        );
        assert_eq!(
            defer_resize_test_frame(&mut state, 11, &[8]),
            Some((101, BTreeSet::from([6]), Vec::new()))
        );
        assert_eq!(visible_resize_sequences(&state), [(7, 5), (8, 5)]);
        assert_eq!(state.deferred_actor_frames.len(), 2);

        insert_resize_test_frame(&mut state, 12, 102, [(8, resize_test_scene(8, Some(6)))]);
        assert_eq!(
            claim_actor_frame_for_preparation_in_state(&mut state, 12, &[8]).unwrap(),
            ActorFramePreparationClaim::PrepareDeferred
        );
        assert_eq!(
            defer_resize_test_frame(&mut state, 12, &[8]),
            Some((102, BTreeSet::from([6]), Vec::new()))
        );
        assert_eq!(visible_resize_sequences(&state), [(7, 5), (8, 5)]);

        insert_resize_test_frame(&mut state, 13, 103, [(7, resize_test_scene(9, Some(6)))]);
        assert_eq!(
            claim_actor_frame_for_preparation_in_state(&mut state, 13, &[7]).unwrap(),
            ActorFramePreparationClaim::PrepareDeferred
        );
        assert_eq!(
            defer_resize_test_frame(&mut state, 13, &[7]),
            Some((103, BTreeSet::from([6]), Vec::new()))
        );
        assert_eq!(visible_resize_sequences(&state), [(7, 5), (8, 5)]);

        complete_resize_epoch_state(&mut state, 6, true);
        assert_eq!(visible_resize_sequences(&state), [(7, 9), (8, 8)]);
        assert!(state.active_resize_epochs.is_empty());
        assert!(state.staged_panes.is_empty());
        assert!(state.deferred_panes.is_empty());
        assert!(state.deferred_actor_frames.is_empty());
        assert!(state.pending_actor_frames.is_empty());
    }

    #[test]
    fn active_resize_actor_frames_preserve_and_replay_a_mixed_batch_without_partial_promotion() {
        let mut state = GraphicsState::default();
        state.panes.insert(7, resize_test_scene(5, None));
        state.panes.insert(8, resize_test_scene(5, None));
        state.visible_panes.extend([7, 8]);
        state.active_resize_epochs.extend([(7, 6), (8, 6)]);
        insert_resize_test_frame(
            &mut state,
            20,
            200,
            [
                (7, resize_test_scene(6, None)),
                (8, resize_test_scene(6, Some(6))),
            ],
        );

        assert_eq!(
            claim_actor_frame_for_preparation_in_state(&mut state, 20, &[7, 8]).unwrap(),
            ActorFramePreparationClaim::PrepareDeferred
        );
        assert_eq!(
            defer_resize_test_frame(&mut state, 20, &[7, 8]),
            Some((200, BTreeSet::from([6]), Vec::new()))
        );
        assert_eq!(visible_resize_sequences(&state), [(7, 5), (8, 5)]);
        let deferred = state.deferred_actor_frames.get(&20).unwrap();
        assert_eq!(deferred.scenes.len(), 2);
        assert_eq!(deferred.scenes.get(&7).map(|scene| scene.sequence), Some(6));
        assert_eq!(
            deferred.scenes.get(&8).and_then(|scene| scene.resize_epoch),
            Some(6)
        );
        assert!(state.deferred_panes.is_empty());
        assert!(state.staged_panes.is_empty());
        assert!(state.pending_actor_frames.is_empty());
        assert!(
            actor_frame_snapshots_from_state(&state, 20, [7, 8])
                .unwrap()
                .is_none()
        );
        assert_eq!(state.actor_frames_promoted, 0);

        // Successful resolution makes the previously tagged member ordinary,
        // then replays the preserved two-pane transaction as one claim.
        complete_resize_epoch_state(&mut state, 6, true);
        assert!(state.deferred_actor_frames.is_empty());
        assert_eq!(visible_resize_sequences(&state), [(7, 6), (8, 6)]);
        assert!(state.pending_actor_frames.is_empty());

        let mut unrelated = GraphicsState::default();
        unrelated.panes.insert(7, resize_test_scene(5, None));
        unrelated.panes.insert(9, resize_test_scene(5, None));
        unrelated.visible_panes.extend([7, 9]);
        unrelated.active_resize_epochs.insert(7, 6);
        insert_resize_test_frame(
            &mut unrelated,
            21,
            201,
            [
                (7, resize_test_scene(6, None)),
                (9, resize_test_scene(6, None)),
            ],
        );
        assert_eq!(
            claim_actor_frame_for_preparation_in_state(&mut unrelated, 21, &[7, 9]).unwrap(),
            ActorFramePreparationClaim::PrepareDeferred
        );
        assert_eq!(
            defer_resize_test_frame(&mut unrelated, 21, &[7, 9]),
            Some((201, BTreeSet::from([6]), Vec::new()))
        );
        assert_eq!(
            unrelated.deferred_actor_frames.get(&21).map(|frame| frame
                .scenes
                .keys()
                .copied()
                .collect::<Vec<_>>()),
            Some(vec![7, 9])
        );
        abort_resize_epoch_state(&mut unrelated, 6);
        assert_eq!(visible_resize_sequences(&unrelated), [(7, 6), (9, 6)]);
    }

    #[test]
    fn active_resize_actor_frames_recover_deferred_content_after_abort_or_degradation() {
        for degraded in [false, true] {
            let mut state = GraphicsState::default();
            state.panes.insert(7, resize_test_scene(5, None));
            state.panes.insert(8, resize_test_scene(5, None));
            state.active_resize_epochs.extend([(7, 6), (8, 6)]);
            insert_resize_test_frame(&mut state, 30, 300, [(7, resize_test_scene(6, None))]);
            insert_resize_test_frame(&mut state, 31, 301, [(8, resize_test_scene(7, None))]);
            assert_eq!(
                claim_actor_frame_for_preparation_in_state(&mut state, 30, &[7]).unwrap(),
                ActorFramePreparationClaim::PrepareDeferred
            );
            assert_eq!(
                defer_resize_test_frame(&mut state, 30, &[7]),
                Some((300, BTreeSet::from([6]), Vec::new()))
            );
            assert_eq!(
                claim_actor_frame_for_preparation_in_state(&mut state, 31, &[8]).unwrap(),
                ActorFramePreparationClaim::PrepareDeferred
            );
            assert_eq!(
                defer_resize_test_frame(&mut state, 31, &[8]),
                Some((301, BTreeSet::from([6]), Vec::new()))
            );

            if degraded {
                complete_resize_epoch_state(&mut state, 6, false);
            } else {
                abort_resize_epoch_state(&mut state, 6);
            }
            assert!(state.deferred_actor_frames.is_empty());
            assert_eq!(visible_resize_sequences(&state), [(7, 6), (8, 7)]);
            assert!(state.active_resize_epochs.is_empty());
            assert!(state.deferred_panes.is_empty());
            assert!(state.staged_panes.is_empty());
        }
    }

    #[test]
    fn deferred_actor_frame_waits_for_every_blocker_epoch_before_recovery() {
        let deferred_state = || {
            let mut state = GraphicsState::default();
            state.panes.insert(7, resize_test_scene(5, None));
            state.panes.insert(8, resize_test_scene(5, None));
            state.panes.insert(9, resize_test_scene(5, None));
            state.active_resize_epochs.extend([(7, 6), (8, 7)]);
            insert_resize_test_frame(
                &mut state,
                35,
                305,
                [
                    (7, resize_test_scene(6, None)),
                    (8, resize_test_scene(6, None)),
                    (9, resize_test_scene(6, Some(6))),
                ],
            );
            assert_eq!(
                claim_actor_frame_for_preparation_in_state(&mut state, 35, &[7, 8, 9]).unwrap(),
                ActorFramePreparationClaim::PrepareDeferred
            );
            assert_eq!(
                defer_resize_test_frame(&mut state, 35, &[7, 8, 9]),
                Some((305, BTreeSet::from([6, 7]), Vec::new()))
            );
            state
        };

        let mut state = deferred_state();
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut state, 6, false, false);
        assert_eq!(plan.retain, BTreeSet::from([35]));
        assert!(plan.commit.is_empty());
        abort_resize_epoch_state(&mut state, 6);
        assert_eq!(visible_resize_sequences(&state), [(7, 5), (8, 5), (9, 5)]);
        assert_eq!(
            state
                .deferred_actor_frames
                .get(&35)
                .map(|frame| frame.blocker_epochs.clone()),
            Some(BTreeSet::from([7]))
        );
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut state, 7, true, false);
        assert_eq!(
            plan.commit.get(&35).cloned(),
            Some(BTreeMap::from([(7, 6), (8, 6)]))
        );
        complete_resize_epoch_state(&mut state, 7, true);
        assert_eq!(visible_resize_sequences(&state), [(7, 6), (8, 6), (9, 5)]);
        assert!(state.deferred_actor_frames.is_empty());

        let mut reverse = deferred_state();
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut reverse, 6, true, false);
        assert_eq!(plan.retain, BTreeSet::from([35]));
        complete_resize_epoch_state(&mut reverse, 6, true);
        assert_eq!(visible_resize_sequences(&reverse), [(7, 5), (8, 5), (9, 5)]);
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut reverse, 7, false, false);
        assert_eq!(
            plan.commit.get(&35).cloned(),
            Some(BTreeMap::from([(7, 6), (8, 6), (9, 6)]))
        );
        abort_resize_epoch_state(&mut reverse, 7);
        assert_eq!(visible_resize_sequences(&reverse), [(7, 6), (8, 6), (9, 6)]);
        assert!(reverse.deferred_actor_frames.is_empty());
    }

    #[test]
    fn partial_visibility_preserves_complete_facet_preparation_membership() {
        let mut state = GraphicsState::default();
        state.visible_panes.insert(7);
        state.pending_actor_frames.insert(
            41,
            PendingGraphicsActorFrame {
                actor: ActorScope::new("facet-visibility", 9),
                token: 301,
                revision: 41,
                scenes: BTreeMap::from([
                    (7, resize_test_scene(8, None)),
                    (8, resize_test_scene(8, None)),
                ]),
                ready_notified: false,
                required_panes: Some(BTreeSet::from([7, 8])),
                blocker_epochs: BTreeSet::new(),
                facet_receipt: None,
                facet_atomic: true,
            },
        );
        let notifications = ready_actor_frame_notifications(&mut state);
        assert_eq!(notifications.len(), 1);
        let panes = notifications
            .iter()
            .flat_map(|(_, _, changes)| changes.iter().map(|change| change.pane))
            .collect::<BTreeSet<_>>();
        assert_eq!(panes, BTreeSet::from([7, 8]));
        assert!(ready_actor_frame_notifications(&mut state).is_empty());
        state.visible_panes.clear();
        if let Some(frame) = state.pending_actor_frames.get_mut(&41) {
            frame.ready_notified = false;
        }
        assert!(ready_actor_frame_notifications(&mut state).is_empty());
    }

    #[test]
    fn cleanup_never_releases_an_uncommitted_facet_scene() {
        let mut state = GraphicsState::default();
        state.deferred_actor_frames.insert(
            41,
            PendingGraphicsActorFrame {
                actor: ActorScope::new("facet-cleanup", 9),
                token: 301,
                revision: 41,
                scenes: BTreeMap::from([(7, resize_test_scene(8, Some(6)))]),
                ready_notified: true,
                required_panes: Some(BTreeSet::from([7])),
                blocker_epochs: BTreeSet::from([6]),
                facet_receipt: None,
                facet_atomic: true,
            },
        );

        let released =
            release_deferred_actor_frames_for_epoch(&mut state, 6, &BTreeSet::from([7]), true);
        assert!(released.is_empty());
        assert!(state.panes.is_empty());
        assert!(state.deferred_actor_frames.is_empty());
    }

    #[test]
    fn degraded_native_promotion_preserves_a_complete_prepared_facet_but_abort_discards_it() {
        for promote_prepared in [false, true] {
            let mut state = GraphicsState::default();
            state.panes.insert(7, resize_test_scene(1, None));
            state.panes.insert(8, resize_test_scene(1, None));
            state.deferred_actor_frames.insert(
                41,
                PendingGraphicsActorFrame {
                    actor: ActorScope::new("facet-recovery", 9),
                    token: 301,
                    revision: 41,
                    scenes: BTreeMap::from([
                        (7, resize_test_scene(8, Some(6))),
                        (8, resize_test_scene(8, Some(6))),
                    ]),
                    ready_notified: true,
                    required_panes: Some(BTreeSet::from([7, 8])),
                    blocker_epochs: BTreeSet::from([6]),
                    facet_receipt: None,
                    facet_atomic: true,
                },
            );
            let plan = deferred_actor_frame_resolution_plan_in_state(
                &mut state,
                6,
                false,
                promote_prepared,
            );
            if promote_prepared {
                assert_eq!(
                    plan.commit.get(&41),
                    Some(&BTreeMap::from([(7, 8), (8, 8)]))
                );
                assert_eq!(visible_resize_sequences(&state), [(7, 8), (8, 8)]);
                assert!(plan.cancelled_tokens.is_empty());
            } else {
                assert!(plan.commit.get(&41).is_some_and(BTreeMap::is_empty));
                assert_eq!(visible_resize_sequences(&state), [(7, 1), (8, 1)]);
                assert_eq!(plan.cancelled_tokens, vec![301]);
            }
            assert!(state.deferred_actor_frames.is_empty());
        }
    }

    #[test]
    fn tagged_only_epoch_remains_an_atomic_actor_frame_blocker() {
        let deferred_state = || {
            let mut state = GraphicsState::default();
            state.panes.insert(7, resize_test_scene(5, None));
            state.panes.insert(8, resize_test_scene(5, None));
            state.active_resize_epochs.extend([(7, 6), (8, 7)]);
            insert_resize_test_frame(
                &mut state,
                36,
                306,
                [
                    (7, resize_test_scene(6, None)),
                    (8, resize_test_scene(7, Some(7))),
                ],
            );
            assert_eq!(
                claim_actor_frame_for_preparation_in_state(&mut state, 36, &[7, 8]).unwrap(),
                ActorFramePreparationClaim::PrepareDeferred
            );
            assert_eq!(
                defer_resize_test_frame(&mut state, 36, &[7, 8]),
                Some((306, BTreeSet::from([6, 7]), Vec::new()))
            );
            state
        };

        let mut success = deferred_state();
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut success, 6, true, false);
        assert_eq!(plan.retain, BTreeSet::from([36]));
        assert!(plan.commit.is_empty());
        complete_resize_epoch_state(&mut success, 6, true);
        assert_eq!(visible_resize_sequences(&success), [(7, 5), (8, 5)]);
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut success, 7, true, false);
        assert_eq!(
            plan.commit.get(&36).cloned(),
            Some(BTreeMap::from([(7, 6), (8, 7)]))
        );
        complete_resize_epoch_state(&mut success, 7, true);
        assert_eq!(visible_resize_sequences(&success), [(7, 6), (8, 7)]);
        assert!(success.deferred_actor_frames.is_empty());

        for degraded in [false, true] {
            let mut recovery = deferred_state();
            let plan =
                deferred_actor_frame_resolution_plan_in_state(&mut recovery, 7, false, false);
            assert_eq!(plan.retain, BTreeSet::from([36]));
            assert!(plan.commit.is_empty());
            if degraded {
                complete_resize_epoch_state(&mut recovery, 7, false);
            } else {
                abort_resize_epoch_state(&mut recovery, 7);
            }
            assert_eq!(visible_resize_sequences(&recovery), [(7, 5), (8, 5)]);
            assert_eq!(
                recovery.deferred_actor_frames.get(&36).map(|frame| frame
                    .scenes
                    .keys()
                    .copied()
                    .collect::<Vec<_>>()),
                Some(vec![7])
            );
            let plan = deferred_actor_frame_resolution_plan_in_state(&mut recovery, 6, true, false);
            assert_eq!(
                plan.commit.get(&36).cloned(),
                Some(BTreeMap::from([(7, 6)]))
            );
            complete_resize_epoch_state(&mut recovery, 6, true);
            assert_eq!(visible_resize_sequences(&recovery), [(7, 6), (8, 5)]);
            assert!(recovery.deferred_actor_frames.is_empty());
        }
    }

    #[test]
    fn active_resize_actor_frame_retirement_returns_the_exact_token_for_cancellation() {
        let scope = ActorScope::new("resize-token", 2);
        let predictor = FramePredictor::new(16_666_667);
        let mut scheduler = RenderScheduler::default();
        assert!(scheduler.register(scope.clone()));
        assert!(scheduler.set_relevant_panes(&scope, [7]));
        assert!(scheduler.request_render(&scope));
        assert!(scheduler.arm(&scope, 0, &predictor, 1_000_000));
        let deadline = scheduler.next_deadline_ns().unwrap();
        let token = scheduler.dispatch_due(deadline).pop().unwrap().1.token;
        scheduler.finish(token, deadline, &[7]).unwrap();

        let mut state = GraphicsState::default();
        state.panes.insert(7, resize_test_scene(5, None));
        state.active_resize_epochs.insert(7, 6);
        state.pending_actor_frames.insert(
            40,
            PendingGraphicsActorFrame {
                actor: scope,
                token: token.0,
                revision: 40,
                scenes: BTreeMap::from([(7, resize_test_scene(6, None))]),
                ready_notified: true,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
                facet_receipt: None,
                facet_atomic: false,
            },
        );
        assert_eq!(
            claim_actor_frame_for_preparation_in_state(&mut state, 40, &[7]).unwrap(),
            ActorFramePreparationClaim::PrepareDeferred
        );
        let deferred = defer_prepared_actor_frame_in_state(&mut state, 40, &[7])
            .unwrap()
            .unwrap();
        assert_eq!(deferred.token, token.0);
        assert_eq!(deferred.blocker_epochs, BTreeSet::from([6]));
        assert!(deferred.superseded_revisions.is_empty());
        assert!(scheduler.cancel_accepted(token));
        assert!(scheduler.completed_traces().iter().any(|trace| {
            trace.token == token
                && trace.settlement == Some(crate::render_scheduler::Settlement::Cancelled)
        }));
    }

    #[test]
    fn actor_host_teardown_clears_live_resources_and_preserves_history() {
        let actor = "terminal-ui";
        let panes = BTreeSet::from([7, 8]);
        let scope = ActorScope::new(actor, 11);
        let scene = |pane| PaneScene {
            generation: 11,
            sequence: pane,
            resize_epoch: None,
            terminal_input_sequence: Some(pane),
            packet: Packet::default().into(),
            facet_frame: None,
        };
        let mut state = GraphicsState {
            accepted_packets: 9,
            text_layouts_created: 5,
            text_layouts_dropped: 2,
            ..GraphicsState::default()
        };
        for (handle, pane) in [(1, 7), (2, 8)] {
            let mut layout = pending_text_entry();
            layout.pane = pane;
            state.text_layouts.insert(handle, layout);
            state.images.insert(
                handle,
                ImageResource {
                    pane,
                    generation: 11,
                    width: 1,
                    height: 1,
                    linear_premultiplied_rgba8: Arc::from([0_u8; 4]),
                },
            );
            state.panes.insert(pane, scene(pane));
            state.latest_complete_state_watermarks.insert(pane, pane);
        }
        state.actor_panes.insert(scope.clone(), panes.clone());
        state.pending_actor_frames.insert(
            1,
            PendingGraphicsActorFrame {
                actor: scope,
                token: 4,
                revision: 1,
                scenes: BTreeMap::from([(7, scene(7)), (8, scene(8))]),
                ready_notified: true,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
                facet_receipt: None,
                facet_atomic: false,
            },
        );

        // This is the exact state-release path used by ActorPresentationHost::drop.
        // The pre-shutdown snapshot is live, while a fresh post-drop snapshot is
        // empty without erasing the lifetime counters reported alongside it.
        assert_eq!(state.text_layouts.len(), 2);
        assert_eq!(state.text_layouts_created, 5);
        assert_eq!(state.text_layouts_dropped, 2);
        assert_eq!(state.accepted_packets, 9);

        release_actor_pane_state(&mut state, actor, 11, &panes);

        assert!(state.text_layouts.is_empty());
        assert!(state.images.is_empty());
        assert!(state.panes.is_empty());
        assert!(state.latest_complete_state_watermarks.is_empty());
        assert!(state.pending_actor_frames.is_empty());
        assert!(state.actor_panes.is_empty());
        assert_eq!(state.text_layouts_created, 5);
        assert_eq!(state.text_layouts_dropped, 4);
        assert_eq!(state.accepted_packets, 9);
    }

    #[test]
    fn complete_state_watermarks_coalesce_and_report_the_exact_draw_state() {
        let mut state = GraphicsState::default();
        state.visible_panes.insert(7);
        for watermark in 1..=720 {
            record_complete_state_watermark(&mut state, 7, watermark);
        }
        assert_eq!(state.latest_complete_state_watermarks.get(&7), Some(&720));
        assert_eq!(validate_state_watermark(&state, 7, Some(720)), Ok(()));
        assert_eq!(
            validate_state_watermark(&state, 7, Some(721)),
            Err(PresentationError::InvalidPacket)
        );

        // A conservative older draw stays older. The host never promotes it
        // to the latest admitted state and therefore cannot falsely drain
        // correlations for state that the guest did not render.
        assert_eq!(validate_state_watermark(&state, 7, Some(719)), Ok(()));
        state.pending_actor_frames.insert(
            1,
            PendingGraphicsActorFrame {
                actor: ActorScope::new("watermark", 1),
                token: 11,
                revision: 1,
                scenes: BTreeMap::from([(
                    7,
                    PaneScene {
                        generation: 1,
                        sequence: 1,
                        resize_epoch: None,
                        terminal_input_sequence: Some(719),
                        packet: Packet::default().into(),
                        facet_frame: None,
                    },
                )]),
                ready_notified: false,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
                facet_receipt: None,
                facet_atomic: false,
            },
        );
        let notifications = ready_actor_frame_notifications(&mut state);
        assert_eq!(notifications.len(), 1);
        assert_eq!(
            notifications
                .first()
                .and_then(|(_, _, scenes)| scenes.first())
                .and_then(|scene| scene.terminal_input_sequence),
            Some(719)
        );
    }

    #[test]
    fn state_watermark_validation_is_pane_scoped_optional_and_token_preserving() {
        let mut state = GraphicsState::default();
        record_complete_state_watermark(&mut state, 7, 20);
        record_complete_state_watermark(&mut state, 8, 40);
        assert_eq!(validate_state_watermark(&state, 7, None), Ok(()));
        assert_eq!(validate_state_watermark(&state, 7, Some(20)), Ok(()));
        assert_eq!(validate_state_watermark(&state, 8, Some(40)), Ok(()));
        assert_eq!(
            validate_state_watermark(&state, 8, Some(41)),
            Err(PresentationError::InvalidPacket)
        );
        assert_eq!(
            validate_state_watermark(&state, 9, Some(1)),
            Err(PresentationError::InvalidPacket)
        );

        let preparation_calls = std::cell::Cell::new(0_u64);
        let invalid_mixed_batch = vec![
            ActorFrameDraw {
                pane: 7,
                resize_epoch: None,
                state_watermark: Some(20),
                packet: Vec::new(),
            },
            ActorFrameDraw {
                pane: 8,
                resize_epoch: None,
                state_watermark: Some(41),
                packet: Vec::new(),
            },
        ];
        assert_eq!(
            validate_actor_frame_before_preparation(
                invalid_mixed_batch,
                |draw| validate_state_watermark(&state, draw.pane, draw.state_watermark),
                |_draw| {
                    preparation_calls.set(preparation_calls.get().saturating_add(1));
                    Ok(())
                },
            ),
            Err(PresentationError::InvalidPacket)
        );
        assert_eq!(preparation_calls.get(), 0);

        // The production frame path performs all watermark validation before
        // scheduler.finish. Model that boundary directly: a future watermark
        // in one member of a mixed-pane batch leaves its dispatched token
        // valid for a corrected retry.
        let scope = ActorScope::new("watermark", 1);
        let mut scheduler = RenderScheduler::default();
        let predictor = FramePredictor::new(16_666_667);
        assert!(scheduler.register(scope.clone()));
        assert!(scheduler.set_relevant_panes(&scope, [7, 8]));
        assert!(scheduler.request_render(&scope));
        assert!(scheduler.arm(&scope, 0, &predictor, 1_000_000));
        let deadline = scheduler.next_deadline_ns().unwrap();
        let token = scheduler.dispatch_due(deadline).pop().unwrap().1.token;
        assert_eq!(validate_state_watermark(&state, 7, Some(20)), Ok(()));
        assert_eq!(
            validate_state_watermark(&state, 8, Some(41)),
            Err(PresentationError::InvalidPacket)
        );
        assert!(scheduler.finish(token, deadline, &[7, 8]).is_ok());
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
    fn generation_cancel_and_renderer_replay_do_not_accept_stale_tokens() {
        let old = ActorScope::new("replay", 1);
        let new = ActorScope::new("replay", 2);
        let mut scheduler = RenderScheduler::default();
        let predictor = FramePredictor::new(16_666_667);
        assert!(scheduler.register(old.clone()));
        assert!(scheduler.set_relevant_panes(&old, [7]));
        assert!(scheduler.request_render(&old));
        assert!(scheduler.arm(&old, 0, &predictor, 1_000_000));
        let deadline = scheduler.next_deadline_ns().unwrap();
        let stale = scheduler.dispatch_due(deadline).pop().unwrap().1.token;

        let mut state = GraphicsState::default();
        state.visible_panes.insert(7);
        state.pending_actor_frames.insert(
            1,
            PendingGraphicsActorFrame {
                actor: old.clone(),
                token: stale.0,
                revision: 1,
                scenes: BTreeMap::from([(
                    7,
                    PaneScene {
                        generation: 1,
                        sequence: 1,
                        resize_epoch: None,
                        terminal_input_sequence: None,
                        packet: Packet::default().into(),
                        facet_frame: None,
                    },
                )]),
                ready_notified: false,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
                facet_receipt: None,
                facet_atomic: false,
            },
        );
        scheduler.cancel_generation(&old);
        state
            .pending_actor_frames
            .retain(|_revision, frame| frame.actor.name != old.name);
        assert!(scheduler.register(new.clone()));
        assert!(scheduler.set_relevant_panes(&new, [7]));
        assert_eq!(
            scheduler.finish(stale, u64::MAX, &[7]),
            Err(crate::render_scheduler::FinishError::InvalidToken)
        );
        assert!(state.pending_actor_frames.is_empty());

        state.pending_actor_frames.insert(
            2,
            PendingGraphicsActorFrame {
                actor: new,
                token: 22,
                revision: 2,
                scenes: BTreeMap::from([(
                    7,
                    PaneScene {
                        generation: 2,
                        sequence: 2,
                        resize_epoch: None,
                        terminal_input_sequence: None,
                        packet: Packet::default().into(),
                        facet_frame: None,
                    },
                )]),
                ready_notified: false,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
                facet_receipt: None,
                facet_atomic: false,
            },
        );
        assert_eq!(ready_actor_frame_notifications(&mut state).len(), 1);
        state
            .pending_actor_frames
            .get_mut(&2)
            .unwrap()
            .ready_notified = false;
        assert_eq!(ready_actor_frame_notifications(&mut state).len(), 1);
    }

    #[test]
    fn retiring_actor_generation_cancels_its_hint_and_keeps_peer_dispatchable() {
        let retiring = ActorScope::new("retiring", 4);
        let peer = ActorScope::new("peer", 9);
        let mut scheduler = RenderScheduler::default();
        let predictor = FramePredictor::new(16_666_667);
        for actor in [&retiring, &peer] {
            assert!(scheduler.register(actor.clone()));
            assert!(scheduler.set_relevant_panes(actor, [actor.generation]));
            assert!(scheduler.request_render(actor));
            assert!(scheduler.arm(actor, 0, &predictor, 1_000_000));
        }

        scheduler.cancel_generation(&retiring);
        let deadline = scheduler.next_deadline_ns().unwrap();
        let dispatched = scheduler.dispatch_due(deadline);
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched.first().map(|(actor, _hint)| actor), Some(&peer));
        assert!(scheduler.actor_progress().iter().all(|progress| {
            (progress.actor.as_str(), progress.generation)
                != (retiring.name.as_str(), retiring.generation)
        }));
    }

    #[test]
    fn rejected_native_promotion_token_does_not_block_replacement_demand() {
        let actor = ActorScope::new("rejected-native", 4);
        let predictor = FramePredictor::new(16_666_667);
        let mut scheduler = RenderScheduler::default();
        assert!(scheduler.register(actor.clone()));
        assert!(scheduler.set_relevant_panes(&actor, [7]));
        assert!(scheduler.request_render(&actor));
        assert!(scheduler.arm(&actor, 0, &predictor, 1_000_000));
        let deadline = scheduler.next_deadline_ns().unwrap();
        let token = scheduler.dispatch_due(deadline).pop().unwrap().1.token;
        scheduler.finish(token, deadline, &[7]).unwrap();

        // This is the token returned beside Stale when receipt commit fails
        // after the bridge has removed the pending frame.
        assert!(scheduler.cancel_accepted(token));
        assert!(scheduler.request_render(&actor));
        scheduler.arm_all(deadline, &predictor, 1_000_000);
        let replacement_deadline = scheduler.next_deadline_ns().unwrap();
        let replacement = scheduler.dispatch_due(replacement_deadline);
        assert_eq!(replacement.len(), 1);
        assert_ne!(replacement.first().unwrap().1.token, token);
    }

    #[test]
    fn replayed_ready_notification_after_promotion_is_stale() -> Result<()> {
        let mut state = GraphicsState::default();
        state.pending_actor_frames.insert(
            9,
            PendingGraphicsActorFrame {
                actor: ActorScope::new("replay", 2),
                token: 22,
                revision: 9,
                scenes: BTreeMap::from([(
                    7,
                    PaneScene {
                        generation: 2,
                        sequence: 3,
                        resize_epoch: None,
                        terminal_input_sequence: None,
                        packet: Packet::default().into(),
                        facet_frame: None,
                    },
                )]),
                ready_notified: true,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
                facet_receipt: None,
                facet_atomic: false,
            },
        );

        anyhow::ensure!(
            actor_frame_snapshots_from_state(&state, 9, [7])?.is_some(),
            "pending revision was reported stale"
        );
        state.pending_actor_frames.remove(&9);
        anyhow::ensure!(
            actor_frame_snapshots_from_state(&state, 9, [7])?.is_none(),
            "promoted revision remained available to its replayed notification"
        );
        Ok(())
    }

    #[test]
    fn actor_frame_disappearance_during_deferred_preparation_is_stale_and_atomic() -> Result<()> {
        let scene = |pane: u64| PaneScene {
            generation: 2,
            sequence: pane + 10,
            resize_epoch: None,
            terminal_input_sequence: None,
            packet: Packet::default().into(),
            facet_frame: None,
        };
        let mut state = GraphicsState::default();
        state.pending_actor_frames.insert(
            9,
            PendingGraphicsActorFrame {
                actor: ActorScope::new("deferred-preparation", 2),
                token: 22,
                revision: 9,
                scenes: BTreeMap::from([(7, scene(7)), (8, scene(8))]),
                ready_notified: true,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
                facet_receipt: None,
                facet_atomic: false,
            },
        );

        // The first ready event snapshots the complete frame and defers while
        // asynchronous graphics preparation runs.
        let prepared_snapshots = actor_frame_snapshots_from_state(&state, 9, [7, 8])?
            .context("pending actor frame disappeared before preparation")?;
        anyhow::ensure!(
            prepared_snapshots
                .iter()
                .map(|snapshot| (snapshot.pane, snapshot.sequence))
                .collect::<Vec<_>>()
                == [(7, 17), (8, 18)],
            "deferred preparation did not retain the complete actor frame"
        );

        // A replayed ready event consumes the revision before the deferred
        // event resumes. The complete two-pane bridge frame promotes once.
        anyhow::ensure!(
            promote_actor_frame_in_state(&mut state, 9, &[7, 8])?
                == (ActorFramePromotion::Claimed, None),
            "replayed ready event did not claim the pending actor frame"
        );
        anyhow::ensure!(
            state
                .panes
                .iter()
                .map(|(pane, scene)| (*pane, scene.sequence))
                .collect::<Vec<_>>()
                == [(7, 17), (8, 18)],
            "claimed actor frame was not promoted atomically"
        );
        let promoted_state = state
            .panes
            .iter()
            .map(|(pane, scene)| (*pane, scene.sequence))
            .collect::<Vec<_>>();

        // Resuming with the prepared snapshots is expected staleness. It must
        // neither partially promote nor mutate the already-promoted frame.
        anyhow::ensure!(
            promote_actor_frame_in_state(&mut state, 9, &[7, 8])?
                == (ActorFramePromotion::Stale, None),
            "resumed preparation did not report the disappeared revision as stale"
        );
        anyhow::ensure!(
            state
                .panes
                .iter()
                .map(|(pane, scene)| (*pane, scene.sequence))
                .collect::<Vec<_>>()
                == promoted_state,
            "stale preparation mutated the promoted bridge frame"
        );
        anyhow::ensure!(state.actor_frames_promoted == 1);
        anyhow::ensure!(state.actor_frames_promoted_multi_pane == 1);
        anyhow::ensure!(state.actor_frame_partial_promotion_violations == 0);
        Ok(())
    }

    #[test]
    fn resize_retirement_discards_a_deferred_mixed_actor_frame_atomically() -> Result<()> {
        let pending_state = || {
            let scene = |pane: u64, resize_epoch| PaneScene {
                generation: 2,
                sequence: pane + 10,
                resize_epoch,
                terminal_input_sequence: None,
                packet: Packet::default().into(),
                facet_frame: None,
            };
            let mut state = GraphicsState::default();
            state.active_resize_epochs.insert(7, 41);
            state.pending_actor_frames.insert(
                9,
                PendingGraphicsActorFrame {
                    actor: ActorScope::new("deferred-resize", 2),
                    token: 22,
                    revision: 9,
                    scenes: BTreeMap::from([(7, scene(7, Some(41))), (8, scene(8, None))]),
                    ready_notified: true,
                    required_panes: None,
                    blocker_epochs: BTreeSet::new(),
                    facet_receipt: None,
                    facet_atomic: false,
                },
            );
            state
        };

        for complete in [false, true] {
            let mut state = pending_state();
            let prepared = actor_frame_snapshots_from_state(&state, 9, [7, 8])?
                .context("actor frame disappeared before asynchronous preparation")?;
            anyhow::ensure!(prepared.len() == 2);

            if complete {
                complete_resize_epoch_state(&mut state, 41, true);
            } else {
                abort_resize_epoch_state(&mut state, 41);
            }
            anyhow::ensure!(
                retire_pending_actor_frames_for_epoch(&mut state, 41) == [22],
                "resize retirement did not cancel the complete actor token"
            );
            anyhow::ensure!(
                actor_frame_snapshots_from_state(&state, 9, [7, 8])?.is_none(),
                "late preparation resume retained part of the retired actor frame"
            );
            anyhow::ensure!(state.panes.is_empty());
            anyhow::ensure!(state.pending_actor_frames.is_empty());
        }
        Ok(())
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
                packet: Packet::default().into(),
                facet_frame: None,
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
            packet: Packet::default().into(),
            facet_frame: None,
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
            packet: Packet::default().into(),
            facet_frame: None,
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
            packet: Packet::default().into(),
            facet_frame: None,
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
                packet: Packet::default().into(),
                facet_frame: None,
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
                packet: text_packet(&[1]).into(),
                facet_frame: None,
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
                packet: Packet::default().into(),
                facet_frame: None,
            },
        );
        state.staged_panes.insert(
            (7, 41),
            PaneScene {
                generation: 11,
                sequence: 2,
                resize_epoch: Some(41),
                terminal_input_sequence: None,
                packet: Packet::default().into(),
                facet_frame: None,
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
                packet: text_packet(&[1]).into(),
                facet_frame: None,
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
            packet: Packet::default().into(),
            facet_frame: None,
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
