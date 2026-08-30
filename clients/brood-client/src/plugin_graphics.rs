use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
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
};
use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FeatureTag, FontFeatures, FontSystem, Metrics, Shaping,
    Style as CosmicStyle, SwashCache, SwashContent, Weight, Wrap,
};
use serde::Serialize;
use winit::event_loop::EventLoopProxy;

use crate::{
    ClientEvent,
    render_scheduler::{
        ActorSchedulerProgress, ActorScope, COLD_HOST_COST_NS, DIAGNOSTIC_WARMUP_OPPORTUNITIES,
        FramePredictor, MAX_RELEVANT_PANES, MAX_RENDER_ACTORS, RenderScheduler, RenderToken,
        SchedulerMetrics,
    },
};

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESOURCE_DIMENSION: u32 = 4_096;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_TEXT_LAYOUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESOURCES_PER_PANE: usize = 1_024;
const MAX_RESOURCE_BYTES_PER_PANE: usize = 64 * 1024 * 1024;
const GLYPH_RASTER_QUEUE_CAPACITY: usize = 128;
const COLD_WINIT_REDRAW_HOP_NS: u64 = 1_000_000;
const MAX_WINIT_REDRAW_HOP_NS: u64 = 50_000_000;
const DEFAULT_RASTER_SCALE: f32 = 1.0;

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
    packet: Packet,
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
    text: Arc<str>,
    options: TextLayoutOptions,
    raster_scale: f32,
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
    actor_frames_accepted: u64,
    actor_frames_ready: u64,
    actor_frames_promoted: u64,
    actor_frames_promoted_multi_pane: u64,
    actor_frame_partial_promotion_violations: u64,
    visibility_changes: u64,
    max_visible_panes: usize,
    raster_scale: f32,
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
            if !required.is_subset(&state.visible_panes) {
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
                pane,
                sequence: scene.sequence,
                resize_epoch: scene.resize_epoch,
                packet: scene.packet.clone(),
                images: resources.images,
                text_layouts: resources.text_layouts,
                glyphs: resources.glyphs,
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn promote_actor_frame_in_state(
    state: &mut GraphicsState,
    revision: u64,
    panes: &[u64],
) -> Result<ActorFramePromotion> {
    let Some(mut frame) = state.pending_actor_frames.remove(&revision) else {
        return Ok(ActorFramePromotion::Stale);
    };
    let promoted_panes = panes.iter().copied().collect::<BTreeSet<_>>();
    if promoted_panes.len() != panes.len()
        || promoted_panes
            .iter()
            .any(|pane| !frame.scenes.contains_key(pane))
    {
        state.actor_frame_partial_promotion_violations = state
            .actor_frame_partial_promotion_violations
            .saturating_add(1);
        state.pending_actor_frames.insert(revision, frame);
        anyhow::bail!("actor frame promotion pane set is invalid");
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
    eprintln!(
        "render scheduling actor-frame promoted actor={} generation={} token={} revision={} pane_count={} panes={panes:?} resize_epochs={resize_epochs:?}",
        actor.name,
        actor.generation,
        token,
        revision,
        panes.len(),
    );
    Ok(ActorFramePromotion::Claimed)
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
        if promote_tagged {
            for scene in frame.scenes.values_mut() {
                if scene.resize_epoch == Some(resize_epoch) {
                    scene.resize_epoch = None;
                }
            }
        } else {
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
            ..GraphicsState::default()
        }));
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
        let mut scheduler = self
            .render_scheduler
            .lock()
            .map_err(|_poisoned| anyhow!("render scheduler is poisoned"))?;
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
            .map(|deadline| self.render_clock_origin + std::time::Duration::from_nanos(deadline)))
    }

    pub fn mark_graphics_ready(
        &self,
        token: u64,
        panes: impl IntoIterator<Item = u64>,
    ) -> CompositionRelease {
        let now = self.render_now_ns();
        if let Ok(mut scheduler) = self.render_scheduler.lock() {
            for pane in panes {
                scheduler.mark_graphics_ready(RenderToken(token), pane, now);
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
        promote_actor_frame_in_state(&mut state, revision, panes)
    }

    /// Retire an actor frame whose complete atomic batch can no longer be
    /// presented. This is used when asynchronous preparation resumes after a
    /// resize epoch in the batch has already been promoted or superseded.
    pub fn discard_actor_frame(&self, revision: u64) -> Result<bool> {
        let token = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?
            .pending_actor_frames
            .remove(&revision)
            .map(|frame| frame.token);
        if let Some(token) = token {
            self.cancel_accepted_tokens([token]);
            return Ok(true);
        }
        Ok(false)
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
    ) -> Result<DeferredActorFrameResolutionPlan> {
        let mut state = self
            .shared
            .lock()
            .map_err(|_poisoned| anyhow!("plugin graphics state is poisoned"))?;
        Ok(deferred_actor_frame_resolution_plan_in_state(
            &mut state,
            resize_epoch,
            promote_tagged,
        ))
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

fn release_actor_pane_state(state: &mut GraphicsState, actor: &str, panes: &BTreeSet<u64>) {
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
        .retain(|_revision, frame| frame.actor.name != actor);
    state
        .deferred_actor_frames
        .retain(|_revision, frame| frame.actor.name != actor);
    state
        .actor_panes
        .retain(|scope, _panes| scope.name != actor);
}

impl ActorPresentationHost {
    pub fn bind_generation(&self, generation: u64) {
        let previous = self.generation.swap(generation, Ordering::AcqRel);
        let scope = ActorScope::new(self.actor.clone(), generation);
        if let Ok(mut state) = self.bridge.shared.lock() {
            release_actor_pane_state(&mut state, &self.actor, &self.panes);
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
}

impl Drop for ActorPresentationHost {
    fn drop(&mut self) {
        if let Ok(mut state) = self.bridge.shared.lock() {
            release_actor_pane_state(&mut state, &self.actor, &self.panes);
        }
        let generation = self.generation.load(Ordering::Acquire);
        if generation != 0
            && let Ok(mut scheduler) = self.bridge.render_scheduler.lock()
        {
            scheduler.cancel_generation(&ActorScope::new(self.actor.clone(), generation));
        }
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
                let packet = Packet::decode(&draw.packet).map_err(|error| match error {
                    PacketError::Limit(_) => PresentationError::LimitReached,
                    PacketError::Invalid(_) => PresentationError::InvalidPacket,
                })?;
                let resources = referenced_resources(&state, draw.pane, actor_generation, &packet)?;
                let candidate = GraphicsSnapshot {
                    pane: draw.pane,
                    sequence: 0,
                    resize_epoch: draw.resize_epoch,
                    packet,
                    images: resources.images,
                    text_layouts: resources.text_layouts,
                    glyphs: resources.glyphs,
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
        let text: Arc<str> = text.into();
        let mut state = self
            .bridge
            .shared
            .lock()
            .map_err(|_poisoned| PresentationError::HostFailure)?;
        if actor_generation != self.generation.load(Ordering::Acquire) {
            return Err(PresentationError::InvalidPane);
        }
        let raster_scale = sanitize_raster_scale(state.raster_scale);
        let handle = allocate_resource_handle(&mut state, pane, actor_generation, byte_len)?;
        state.text_layouts.insert(
            handle,
            TextLayoutEntry {
                pane,
                generation: actor_generation,
                width: plan.width,
                height: plan.height,
                byte_len,
                text,
                options,
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
                let physical = glyph.physical((0.0, run.line_y), raster_scale);
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
            text: Arc::from("x"),
            options: test_text_options(),
            raster_scale: 1.0,
            state: TextLayoutState::Pending,
        }
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
            generation: 2,
            sequence,
            resize_epoch,
            terminal_input_sequence: Some(sequence),
            packet: Packet::default(),
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
            },
        );
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
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut state, 6, false);
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
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut state, 7, true);
        assert_eq!(
            plan.commit.get(&35).cloned(),
            Some(BTreeMap::from([(7, 6), (8, 6)]))
        );
        complete_resize_epoch_state(&mut state, 7, true);
        assert_eq!(visible_resize_sequences(&state), [(7, 6), (8, 6), (9, 5)]);
        assert!(state.deferred_actor_frames.is_empty());

        let mut reverse = deferred_state();
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut reverse, 6, true);
        assert_eq!(plan.retain, BTreeSet::from([35]));
        complete_resize_epoch_state(&mut reverse, 6, true);
        assert_eq!(visible_resize_sequences(&reverse), [(7, 5), (8, 5), (9, 5)]);
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut reverse, 7, false);
        assert_eq!(
            plan.commit.get(&35).cloned(),
            Some(BTreeMap::from([(7, 6), (8, 6), (9, 6)]))
        );
        abort_resize_epoch_state(&mut reverse, 7);
        assert_eq!(visible_resize_sequences(&reverse), [(7, 6), (8, 6), (9, 6)]);
        assert!(reverse.deferred_actor_frames.is_empty());
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
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut success, 6, true);
        assert_eq!(plan.retain, BTreeSet::from([36]));
        assert!(plan.commit.is_empty());
        complete_resize_epoch_state(&mut success, 6, true);
        assert_eq!(visible_resize_sequences(&success), [(7, 5), (8, 5)]);
        let plan = deferred_actor_frame_resolution_plan_in_state(&mut success, 7, true);
        assert_eq!(
            plan.commit.get(&36).cloned(),
            Some(BTreeMap::from([(7, 6), (8, 7)]))
        );
        complete_resize_epoch_state(&mut success, 7, true);
        assert_eq!(visible_resize_sequences(&success), [(7, 6), (8, 7)]);
        assert!(success.deferred_actor_frames.is_empty());

        for degraded in [false, true] {
            let mut recovery = deferred_state();
            let plan = deferred_actor_frame_resolution_plan_in_state(&mut recovery, 7, false);
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
            let plan = deferred_actor_frame_resolution_plan_in_state(&mut recovery, 6, true);
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
            packet: Packet::default(),
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
            },
        );

        // This is the exact state-release path used by ActorPresentationHost::drop.
        // The pre-shutdown snapshot is live, while a fresh post-drop snapshot is
        // empty without erasing the lifetime counters reported alongside it.
        assert_eq!(state.text_layouts.len(), 2);
        assert_eq!(state.text_layouts_created, 5);
        assert_eq!(state.text_layouts_dropped, 2);
        assert_eq!(state.accepted_packets, 9);

        release_actor_pane_state(&mut state, actor, &panes);

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
                        packet: Packet::default(),
                    },
                )]),
                ready_notified: false,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
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
                        packet: Packet::default(),
                    },
                )]),
                ready_notified: false,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
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
                        packet: Packet::default(),
                    },
                )]),
                ready_notified: false,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
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
                        packet: Packet::default(),
                    },
                )]),
                ready_notified: true,
                required_panes: None,
                blocker_epochs: BTreeSet::new(),
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
            packet: Packet::default(),
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
            promote_actor_frame_in_state(&mut state, 9, &[7, 8])? == ActorFramePromotion::Claimed,
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
            promote_actor_frame_in_state(&mut state, 9, &[7, 8])? == ActorFramePromotion::Stale,
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
                packet: Packet::default(),
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
