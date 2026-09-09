use std::{collections::BTreeMap, sync::Mutex};

use brood_graphics::{Color, Command, Packet, Rect};
#[cfg(not(feature = "pure-renderer"))]
use brood_plugin_sdk::bindings;
#[cfg(not(feature = "pure-renderer"))]
use brood_plugin_sdk::graphics::{queue_packet, queue_tagged_packet};
#[cfg(feature = "pure-renderer")]
use brood_plugin_sdk::{Facets, export_visual_plugin, facet_codec, facets};
use brood_plugin_sdk::{
    actor::{send_local, start_global_actor},
    exports::brood::plugin::actor::{
        Command as ActorCommand, Event, Guest, LifecycleEvent, PresentationEvent, WorkerEvent,
        WorkerRequest,
    },
    runtime::sleep,
};
use serde::{Deserialize, Serialize};

const INIT_ACTOR_ID: &str = "brood.client-init";
const FAST_ACTOR_ID: &str = "resize-proof-fast";
const GATED_FAST_ACTOR_ID: &str = "resize-proof-gated-fast";
const SLOW_ACTOR_ID: &str = "resize-proof-slow";
const WINDOW_SLOW_ACTOR_ID: &str = "resize-proof-window-slow";
const STALL_ACTOR_ID: &str = "resize-proof-stall";
const STALL_AFTER_FIRST_ACTOR_ID: &str = "resize-proof-stall-after-first";
const MIDDLE_LAYOUT_ACTOR_ID: &str = "workspace";
const LEAF_LAYOUT_ACTOR_ID: &str = "resize-proof-leaf-layout";
const COMPONENT_FILE: &str = "brood_resize_proof_plugin.component.wasm";
// The burst proof deliberately runs one obsolete cycle and one coalesced final
// cycle. Keep their combined guest delay at 180 ms so the host retains 70 ms
// of its fixed 250 ms transaction budget for hint dispatch and preparation.
const DELAY_MS: u64 = 90;
const _: () = assert!(DELAY_MS * 2 <= 180);
// Window screenshots need a longer observable projected interval, but this
// remains below the host's 250 ms synchronized-resize deadline.
const WINDOW_DELAY_MS: u64 = 180;
// Keep post-timeout recovery out of the frame-locked degraded capture. The
// next tagged recovery epoch cancels this proof-only worker immediately.
const RECOVERY_DELAY_MS: u64 = 500;
const DELAY_PAYLOAD: &[u8] = b"resize-proof-delay-v1";
const WINDOW_DELAY_PAYLOAD: &[u8] = b"resize-proof-window-delay-v1";
const RECOVERY_PAYLOAD: &[u8] = b"resize-proof-recovery-v1";
const MIDDLE_REGION_DELAY_PAYLOAD: &[u8] = b"resize-proof-middle-region-v1";
const LEAF_REGION_DELAY_PAYLOAD: &[u8] = b"resize-proof-leaf-region-v1";
const RECOVERY_WORKER: u64 = 1;
const LEFT_POINTER_BUTTON: u32 = 0x110;
const RIGHT_POINTER_BUTTON: u32 = 0x111;
const INITIAL_BACKGROUND: Color = Color::rgba(0.04, 0.08, 0.18, 1.0);
const RECOVERY_BACKGROUND: Color = Color::rgba(0.08, 0.36, 0.16, 1.0);
const BIT_ZERO: Color = Color::rgba(0.02, 0.02, 0.025, 1.0);
const BIT_ONE: Color = Color::rgba(0.96, 0.97, 0.99, 1.0);
const FAST_MARKER: Color = Color::rgba(0.04, 0.82, 0.92, 1.0);
const SLOW_MARKER: Color = Color::rgba(0.92, 0.12, 0.72, 1.0);
const STALL_MARKER: Color = Color::rgba(0.96, 0.58, 0.08, 1.0);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
enum ProofMode {
    Fast,
    GatedFast,
    Slow,
    WindowSlow,
    StallOnce,
    StallAfterFirst,
    MiddleLayout,
    LeafLayout,
}

impl ProofMode {
    fn for_instance(instance_id: &str) -> Self {
        if instance_id == MIDDLE_LAYOUT_ACTOR_ID {
            Self::MiddleLayout
        } else if instance_id == LEAF_LAYOUT_ACTOR_ID {
            Self::LeafLayout
        } else if instance_id.ends_with("-gated-fast") {
            Self::GatedFast
        } else if instance_id.ends_with("-window-slow") {
            Self::WindowSlow
        } else if instance_id.ends_with("-slow") {
            Self::Slow
        } else if instance_id.ends_with("-stall-after-first") {
            Self::StallAfterFirst
        } else if instance_id.ends_with("-stall") {
            Self::StallOnce
        } else {
            Self::Fast
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Fast | Self::GatedFast => "fast",
            Self::Slow => "slow",
            Self::WindowSlow => "slow",
            Self::StallOnce => "stall-once",
            Self::StallAfterFirst => "stall-after-first",
            Self::MiddleLayout => "middle-layout",
            Self::LeafLayout => "leaf-layout",
        }
    }

    const fn actor_id(self) -> &'static str {
        match self {
            Self::Fast => FAST_ACTOR_ID,
            Self::GatedFast => GATED_FAST_ACTOR_ID,
            Self::Slow => SLOW_ACTOR_ID,
            Self::WindowSlow => WINDOW_SLOW_ACTOR_ID,
            Self::StallOnce => STALL_ACTOR_ID,
            Self::StallAfterFirst => STALL_AFTER_FIRST_ACTOR_ID,
            Self::MiddleLayout => MIDDLE_LAYOUT_ACTOR_ID,
            Self::LeafLayout => LEAF_LAYOUT_ACTOR_ID,
        }
    }

    const fn marker(self) -> Color {
        match self {
            Self::Fast | Self::GatedFast => FAST_MARKER,
            Self::Slow | Self::WindowSlow => SLOW_MARKER,
            Self::StallOnce | Self::StallAfterFirst => STALL_MARKER,
            Self::MiddleLayout => FAST_MARKER,
            Self::LeafLayout => SLOW_MARKER,
        }
    }

    const fn stalls(self) -> bool {
        matches!(self, Self::StallOnce | Self::StallAfterFirst)
    }

    const fn gates_untagged_recovery(self) -> bool {
        matches!(
            self,
            Self::GatedFast | Self::MiddleLayout | Self::LeafLayout
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PaintTarget {
    pane: u64,
    epoch: u64,
    width: f32,
    height: f32,
    scale: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct UntaggedTarget {
    pane: u64,
    width: f32,
    height: f32,
    scale: f32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
struct ProofLayoutLease {
    id: u64,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
struct ProofLayoutSize {
    w: i32,
    #[serde(alias = "height")]
    h: i32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
struct ProofResizeEpoch {
    id: u64,
    lease: u64,
    lease_generation: u64,
    base_layout_revision: u64,
    target_layout_revision: u64,
}

#[derive(Debug, Deserialize)]
struct ProofLayoutMessage {
    #[serde(rename = "type")]
    message_type: String,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    presented_layout_revision: u64,
    lease: ProofLayoutLease,
    #[serde(default)]
    waitable_panes: Vec<u64>,
    #[serde(default)]
    size: ProofLayoutSize,
    #[serde(default)]
    rest: bool,
    #[serde(default)]
    resize_epoch: Option<ProofResizeEpoch>,
    #[serde(default)]
    scale: f32,
}

#[derive(Clone, Debug)]
struct ProofLayoutContext {
    revision: u64,
    presented_layout_revision: u64,
    lease: ProofLayoutLease,
    panes: Vec<u64>,
    size: ProofLayoutSize,
    rest: bool,
}

#[derive(Clone, Debug)]
struct PendingLayoutRegion {
    epoch: ProofResizeEpoch,
    lease: ProofLayoutLease,
    size: ProofLayoutSize,
    scale: f32,
    revision: u64,
    pane: u64,
    has_rest: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
enum FrameIdentity {
    Initial,
    Recovery,
    Epoch(u64),
}

#[cfg(not(feature = "pure-renderer"))]
#[derive(Clone, Copy, Debug, PartialEq)]
enum PendingDraw {
    Tagged(PaintTarget),
    Untagged(UntaggedTarget, FrameIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DelayAction {
    None,
    Spawn { worker: u64, target: PaintTarget },
    Draw(PaintTarget),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryAction {
    Immediate,
    Pending,
    Spawn(u64),
}

#[derive(Debug)]
struct DelayScheduler {
    pending: Option<PaintTarget>,
    active: Option<(u64, PaintTarget)>,
    next_worker: u64,
}

impl DelayScheduler {
    const fn new() -> Self {
        Self {
            pending: None,
            active: None,
            next_worker: 1,
        }
    }

    fn offer(&mut self, target: PaintTarget) -> DelayAction {
        self.pending = Some(target);
        if self.active.is_some() {
            DelayAction::None
        } else {
            self.start_pending()
        }
    }

    fn finish(&mut self, worker: u64) -> DelayAction {
        let Some((active_worker, completed)) = self.active else {
            return DelayAction::None;
        };
        if active_worker != worker {
            return DelayAction::None;
        }
        self.active = None;
        if self.pending == Some(completed) {
            self.pending = None;
            DelayAction::Draw(completed)
        } else {
            self.start_pending()
        }
    }

    fn fail(&mut self, worker: u64) -> DelayAction {
        if self
            .active
            .is_none_or(|(active_worker, _target)| active_worker != worker)
        {
            return DelayAction::None;
        }
        self.active = None;
        self.pending
            .take()
            .map_or(DelayAction::None, DelayAction::Draw)
    }

    fn reset(&mut self) -> Option<u64> {
        self.pending = None;
        self.active.take().map(|(worker, _target)| worker)
    }

    fn start_pending(&mut self) -> DelayAction {
        let Some(target) = self.pending else {
            return DelayAction::None;
        };
        let worker = self.next_worker;
        self.next_worker = self.next_worker.saturating_add(1).max(1);
        self.active = Some((worker, target));
        DelayAction::Spawn { worker, target }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum WindowGateOffer {
    NotArmed,
    Held,
    Replaced { cancelled: PaintTarget },
    Stale { held: PaintTarget },
}

#[derive(Debug)]
struct WindowCaptureGate {
    armed: bool,
    held: Option<PaintTarget>,
    pending_release: Option<PaintTarget>,
}

impl WindowCaptureGate {
    const fn new() -> Self {
        Self {
            armed: false,
            held: None,
            pending_release: None,
        }
    }

    fn arm(&mut self) -> bool {
        if self.armed || self.held.is_some() {
            return false;
        }
        self.armed = true;
        true
    }

    fn offer(&mut self, target: PaintTarget) -> WindowGateOffer {
        if let Some(pending) = self.pending_release {
            if target.epoch <= pending.epoch {
                return WindowGateOffer::Stale { held: pending };
            }
            self.pending_release = None;
        }
        if self.armed {
            self.armed = false;
            self.held = Some(target);
            return WindowGateOffer::Held;
        }
        let Some(held) = self.held else {
            return WindowGateOffer::NotArmed;
        };
        if target.epoch < held.epoch {
            return WindowGateOffer::Stale { held };
        }
        self.held = Some(target);
        WindowGateOffer::Replaced { cancelled: held }
    }

    fn release(&mut self) -> Option<PaintTarget> {
        let target = self.held.take()?;
        self.pending_release = Some(target);
        Some(target)
    }

    fn take_pending_release(&mut self, epoch: u64) -> Option<PaintTarget> {
        if self
            .pending_release
            .is_some_and(|target| target.epoch == epoch)
        {
            self.pending_release.take()
        } else {
            None
        }
    }
}

struct ProofState {
    mode: ProofMode,
    pane: u64,
    width: f32,
    height: f32,
    scale: f32,
    delay: DelayScheduler,
    window_capture_gate: WindowCaptureGate,
    stalled: Option<PaintTarget>,
    recovery_observed: bool,
    recovery_pending: Option<UntaggedTarget>,
    recovery_worker: Option<u64>,
    recovery_delayed: bool,
    tagged_observed: bool,
    first_tagged_submitted: bool,
    #[cfg(not(feature = "pure-renderer"))]
    last_submitted: Option<PaintTarget>,
    error: Option<String>,
    layout: Option<ProofLayoutContext>,
    layout_region_count: u64,
    region_proof_armed: bool,
    region_proof_arm_count: u64,
    pending_layout_regions: BTreeMap<u64, PendingLayoutRegion>,
    #[cfg(not(feature = "pure-renderer"))]
    pending_draw: Option<PendingDraw>,
    #[cfg(feature = "pure-renderer")]
    frame_identity: FrameIdentity,
    #[cfg(feature = "pure-renderer")]
    publication_revision: u64,
    #[cfg(feature = "pure-renderer")]
    published_view: Vec<u8>,
}

impl ProofState {
    const fn new(mode: ProofMode) -> Self {
        Self {
            mode,
            pane: 0,
            width: 1.0,
            height: 1.0,
            scale: 1.0,
            delay: DelayScheduler::new(),
            window_capture_gate: WindowCaptureGate::new(),
            stalled: None,
            recovery_observed: false,
            recovery_pending: None,
            recovery_worker: None,
            recovery_delayed: false,
            tagged_observed: false,
            first_tagged_submitted: false,
            #[cfg(not(feature = "pure-renderer"))]
            last_submitted: None,
            error: None,
            layout: None,
            layout_region_count: 0,
            region_proof_armed: false,
            region_proof_arm_count: 0,
            pending_layout_regions: BTreeMap::new(),
            #[cfg(not(feature = "pure-renderer"))]
            pending_draw: None,
            #[cfg(feature = "pure-renderer")]
            frame_identity: FrameIdentity::Initial,
            #[cfg(feature = "pure-renderer")]
            publication_revision: 0,
            #[cfg(feature = "pure-renderer")]
            published_view: Vec::new(),
        }
    }

    fn stall_until_recovery(&mut self, target: PaintTarget) -> bool {
        if !self.recovery_observed {
            self.stalled = Some(target);
            true
        } else {
            self.stalled = None;
            false
        }
    }

    fn stall_after_optional_warmup(&mut self, target: PaintTarget) -> bool {
        if self.mode == ProofMode::StallAfterFirst && !self.first_tagged_submitted {
            self.first_tagged_submitted = true;
            return false;
        }
        self.stall_until_recovery(target)
    }

    fn schedule_first_recovery(&mut self, target: UntaggedTarget) -> RecoveryAction {
        let had_stalled_epoch = self.stalled.is_some();
        if had_stalled_epoch {
            self.recovery_observed = true;
        }
        if self.recovery_worker.is_some() {
            self.recovery_pending = Some(target);
            return RecoveryAction::Pending;
        }
        if !had_stalled_epoch || self.recovery_delayed {
            return RecoveryAction::Immediate;
        }
        self.recovery_pending = Some(target);
        self.recovery_delayed = true;
        self.recovery_worker = Some(RECOVERY_WORKER);
        RecoveryAction::Spawn(RECOVERY_WORKER)
    }

    fn schedule_gated_recovery(&mut self, target: UntaggedTarget) -> RecoveryAction {
        if self.recovery_worker.is_some() {
            self.recovery_pending = Some(target);
            return RecoveryAction::Pending;
        }
        self.recovery_pending = Some(target);
        self.recovery_worker = Some(RECOVERY_WORKER);
        RecoveryAction::Spawn(RECOVERY_WORKER)
    }

    fn finish_recovery(&mut self, worker: u64) -> Option<UntaggedTarget> {
        if self.recovery_worker != Some(worker) {
            return None;
        }
        self.recovery_worker = None;
        self.stalled = None;
        self.recovery_pending.take()
    }

    fn cancel_recovery(&mut self) -> Option<u64> {
        self.recovery_pending = None;
        self.recovery_worker.take()
    }
}

static STATE: Mutex<ProofState> = Mutex::new(ProofState::new(ProofMode::Fast));

#[cfg(feature = "pure-renderer")]
#[derive(Clone, Copy, Deserialize, Serialize)]
struct RenderView {
    mode: ProofMode,
    identity: FrameIdentity,
}

#[cfg(feature = "pure-renderer")]
#[derive(Deserialize, Serialize)]
struct InteractionMap {
    pane: u64,
    mode: ProofMode,
    identity: FrameIdentity,
}

#[cfg(feature = "pure-renderer")]
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
enum FacetIntent {
    ArmWindowGate {
        pane: u64,
        mode: ProofMode,
        identity: FrameIdentity,
    },
    ReleaseWindowGate {
        pane: u64,
        mode: ProofMode,
        identity: FrameIdentity,
    },
    ArmRegionProof {
        pane: u64,
        mode: ProofMode,
        identity: FrameIdentity,
    },
}

#[cfg(feature = "pure-renderer")]
struct PublicationTurn;

#[cfg(feature = "pure-renderer")]
impl Drop for PublicationTurn {
    fn drop(&mut self) {
        publish_render_view();
    }
}

#[cfg(feature = "pure-renderer")]
fn publish_render_view() {
    let Ok(mut state) = STATE.lock() else { return };
    if state.pane == 0 {
        return;
    }
    let bytes = serde_json::to_vec(&RenderView {
        mode: state.mode,
        identity: state.frame_identity,
    })
    .expect("serialize resize proof render view");
    if bytes == state.published_view {
        return;
    }
    let revision = state.publication_revision.saturating_add(1).max(1);
    if brood_plugin_sdk::publication::publish(revision, &bytes).is_ok() {
        state.publication_revision = revision;
        state.published_view = bytes;
    }
}

struct ResizeProof;

impl Guest for ResizeProof {
    fn dispatch(event: Event) -> Vec<ActorCommand> {
        #[cfg(feature = "pure-renderer")]
        let _publication_turn = PublicationTurn;
        match event {
            Event::Lifecycle(LifecycleEvent::Start(context)) => handle_start(&context.instance_id),
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Presentation(event) => handle_presentation(event),
            Event::Mailbox(message) if message.sender == "layout-host" => {
                handle_layout_mailbox(&message.body)
            }
            Event::Mailbox(message) if message.sender == LEAF_LAYOUT_ACTOR_ID => {
                handle_region_proof_control(&message.body)
            }
            Event::Mailbox(message) if message.sender == WINDOW_SLOW_ACTOR_ID => {
                handle_window_gate_control(&message.body)
            }
            Event::Worker(event) => handle_worker(event),
            #[cfg(feature = "pure-renderer")]
            Event::FacetInput(batch) => handle_facet_input(batch.events),
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        if let Some(delay_ms) = region_delay_ms(&payload) {
            sleep(delay_ms)
                .await
                .map_err(|error| format!("resize proof region delay: {error}"))?;
            return Ok(payload);
        }
        if payload != DELAY_PAYLOAD
            && payload != WINDOW_DELAY_PAYLOAD
            && payload != RECOVERY_PAYLOAD
        {
            return Err("resize proof worker received an unknown payload".to_string());
        }
        let delay_ms = match payload.as_slice() {
            WINDOW_DELAY_PAYLOAD => WINDOW_DELAY_MS,
            RECOVERY_PAYLOAD => RECOVERY_DELAY_MS,
            _ => DELAY_MS,
        };
        sleep(delay_ms)
            .await
            .map_err(|error| format!("resize proof delay: {error}"))?;
        Ok(payload)
    }
}

fn region_delay_payload(mode: ProofMode, delay_ms: u64) -> Vec<u8> {
    let mut payload = match mode {
        ProofMode::MiddleLayout => MIDDLE_REGION_DELAY_PAYLOAD.to_vec(),
        ProofMode::LeafLayout => LEAF_REGION_DELAY_PAYLOAD.to_vec(),
        _ => Vec::new(),
    };
    payload.push(b':');
    payload.extend_from_slice(delay_ms.to_string().as_bytes());
    payload
}

fn region_delay_ms(payload: &[u8]) -> Option<u64> {
    let suffix = payload
        .strip_prefix(MIDDLE_REGION_DELAY_PAYLOAD)
        .or_else(|| payload.strip_prefix(LEAF_REGION_DELAY_PAYLOAD))?
        .strip_prefix(b":")?;
    std::str::from_utf8(suffix).ok()?.parse().ok()
}

fn handle_start(instance_id: &str) -> Vec<ActorCommand> {
    if instance_id == INIT_ACTOR_ID {
        return vec![
            start_global_actor(FAST_ACTOR_ID, COMPONENT_FILE, Vec::new()),
            start_global_actor(SLOW_ACTOR_ID, COMPONENT_FILE, Vec::new()),
            start_global_actor(STALL_ACTOR_ID, COMPONENT_FILE, Vec::new()),
            start_global_actor(STALL_AFTER_FIRST_ACTOR_ID, COMPONENT_FILE, Vec::new()),
            ActorCommand::LifecycleComplete(Ok(())),
        ];
    }
    if let Ok(mut state) = STATE.lock() {
        *state = ProofState::new(ProofMode::for_instance(instance_id));
    }
    vec![ActorCommand::LifecycleComplete(Ok(()))]
}

fn handle_layout_mailbox(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok(message) = serde_json::from_slice::<ProofLayoutMessage>(bytes) else {
        return Vec::new();
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let mode = state.mode;
    if !matches!(mode, ProofMode::MiddleLayout | ProofMode::LeafLayout) {
        return Vec::new();
    }
    match message.message_type.as_str() {
        "layout-state" => {
            let context = ProofLayoutContext {
                revision: message.revision,
                presented_layout_revision: message.presented_layout_revision,
                lease: message.lease,
                panes: message.waitable_panes,
                size: message.size,
                rest: message.rest,
            };
            state.layout = Some(context.clone());
            let pane = state.pane;
            drop(state);
            publish_proof_layout(mode, pane, &context)
        }
        "layout-region-resized" => {
            let Some(epoch) = message.resize_epoch else {
                return Vec::new();
            };
            let Some(context) = state.layout.as_ref().cloned() else {
                return Vec::new();
            };
            if context.lease != message.lease {
                return Vec::new();
            }
            state.layout_region_count = state.layout_region_count.saturating_add(1);
            let response_index = state.layout_region_count;
            let proof_armed = state.region_proof_armed;
            let arm_count = state.region_proof_arm_count;
            let pane = state.pane;
            let pending = PendingLayoutRegion {
                epoch,
                lease: message.lease,
                size: message.size,
                scale: message.scale,
                revision: context.revision,
                pane,
                has_rest: mode == ProofMode::MiddleLayout && context.rest,
            };
            if mode == ProofMode::MiddleLayout && proof_armed && response_index == 4 {
                drop(state);
                return vec![layout_region_diagnostic(
                    "resize-proof-region-withheld",
                    mode,
                    &pending,
                    response_index,
                )];
            }
            let delay_ms = match (mode, proof_armed, arm_count, response_index) {
                (ProofMode::MiddleLayout, true, 1, 1) => 210,
                (ProofMode::MiddleLayout, true, 1, 2) => 150,
                (ProofMode::MiddleLayout, _, _, _) => 80,
                (ProofMode::LeafLayout, _, _, _) => 30,
                _ => 0,
            };
            let worker = 10_000_u64.saturating_add(response_index);
            state.pending_layout_regions.insert(worker, pending.clone());
            drop(state);
            vec![
                layout_region_diagnostic(
                    "resize-proof-region-received",
                    mode,
                    &pending,
                    response_index,
                ),
                ActorCommand::SpawnWorker(WorkerRequest {
                    worker,
                    payload: region_delay_payload(mode, delay_ms),
                }),
            ]
        }
        _ => Vec::new(),
    }
}

fn handle_region_proof_control(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok(message) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Vec::new();
    };
    if message["kind"] != "arm-resize-region-proof" {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if state.mode != ProofMode::MiddleLayout {
        return Vec::new();
    }
    state.layout_region_count = 0;
    state.region_proof_armed = true;
    state.region_proof_arm_count = state.region_proof_arm_count.saturating_add(1);
    state.pending_layout_regions.clear();
    vec![send_local(
        MIDDLE_LAYOUT_ACTOR_ID,
        r#"{"kind":"resize-proof-region-armed"}"#.to_string(),
    )]
}

fn publish_proof_layout(
    mode: ProofMode,
    pane: u64,
    context: &ProofLayoutContext,
) -> Vec<ActorCommand> {
    if pane == 0 || !context.panes.contains(&pane) {
        return Vec::new();
    }
    let mut commands = Vec::new();
    if mode == ProofMode::MiddleLayout && !context.rest {
        let delegated = context
            .panes
            .iter()
            .copied()
            .filter(|candidate| *candidate != pane)
            .collect::<Vec<_>>();
        if !delegated.is_empty() {
            commands.push(ActorCommand::SendHostMessage(
                serde_json::to_vec(&serde_json::json!({
                    "type": "delegate-layout-rest",
                    "lease": {"id": context.lease.id, "generation": context.lease.generation},
                    "revision": context.revision,
                    "actor": LEAF_LAYOUT_ACTOR_ID,
                    "panes": delegated,
                }))
                .unwrap_or_default(),
            ));
            return commands;
        }
    }
    let (layout, rest) = proof_layout_fragment(mode, pane, context.rest);
    commands.push(ActorCommand::SendHostMessage(
        serde_json::to_vec(&serde_json::json!({
            "type": "set-resolved-layout",
            "lease": {"id": context.lease.id, "generation": context.lease.generation},
            "revision": context.revision,
            "layout": layout,
            "rest": rest,
            "focus": pane,
        }))
        .unwrap_or_default(),
    ));
    commands.push(send_local(
        mode.actor_id(),
        format!(
            r#"{{"kind":"resize-proof-layout-state","mode":"{}","pane":{pane},"lease":{},"lease_generation":{},"revision":{},"presented_layout_revision":{},"width":{},"height":{},"rest":{}}}"#,
            mode.label(),
            context.lease.id,
            context.lease.generation,
            context.revision,
            context.presented_layout_revision,
            context.size.w,
            context.size.h,
            context.rest,
        ),
    ));
    commands
}

fn proof_layout_fragment(
    mode: ProofMode,
    pane: u64,
    has_rest: bool,
) -> (serde_json::Value, serde_json::Value) {
    let (offset, size) = if mode == ProofMode::MiddleLayout && has_rest {
        (-1.0, 0.8)
    } else {
        (-1.0, 2.0)
    };
    let layout = serde_json::json!({
        "items": [{
            "id": pane,
            "kind": {"type": "pane", "pane": pane},
            "z": 1.0,
            "transform": {
                "offset": {"u": offset, "v": -1.0},
                "size": {"u": size, "v": 2.0}
            },
            "input": "pane"
        }]
    });
    let rest = if mode == ProofMode::MiddleLayout && has_rest {
        serde_json::json!({
            "z": 0.0,
            "transform": {
                "offset": {"u": -0.2, "v": -1.0},
                "size": {"u": 1.2, "v": 2.0}
            }
        })
    } else {
        serde_json::Value::Null
    };
    (layout, rest)
}

fn layout_region_response(mode: ProofMode, pending: &PendingLayoutRegion) -> ActorCommand {
    let (layout, rest) = proof_layout_fragment(mode, pending.pane, pending.has_rest);
    ActorCommand::SendHostMessage(
        serde_json::to_vec(&serde_json::json!({
            "type": "submit-layout-region",
            "resize_epoch": {
                "id": pending.epoch.id,
                "lease": pending.epoch.lease,
                "lease_generation": pending.epoch.lease_generation,
                "base_layout_revision": pending.epoch.base_layout_revision,
                "target_layout_revision": pending.epoch.target_layout_revision,
            },
            "lease": {"id": pending.lease.id, "generation": pending.lease.generation},
            "inventory_revision": pending.revision,
            "fragment": {"layout": layout, "rest": rest},
            "focus": pending.pane,
        }))
        .unwrap_or_default(),
    )
}

fn layout_region_diagnostic(
    kind: &str,
    mode: ProofMode,
    pending: &PendingLayoutRegion,
    response_index: u64,
) -> ActorCommand {
    send_local(
        mode.actor_id(),
        format!(
            r#"{{"kind":"{kind}","mode":"{}","response_index":{response_index},"pane":{},"epoch":{},"lease":{},"lease_generation":{},"width":{},"height":{},"scale":{:.3}}}"#,
            mode.label(),
            pending.pane,
            pending.epoch.id,
            pending.lease.id,
            pending.lease.generation,
            pending.size.w,
            pending.size.h,
            pending.scale,
        ),
    )
}

fn handle_presentation(event: PresentationEvent) -> Vec<ActorCommand> {
    match event {
        #[cfg(not(feature = "pure-renderer"))]
        PresentationEvent::RenderHint(hint) => {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            let pending = state.pending_draw.take();
            let mode = state.mode;
            drop(state);
            let result = brood_plugin_sdk::graphics::render_with_hint(&hint, || match pending {
                Some(PendingDraw::Tagged(target)) => queue_tagged_packet(
                    target.pane,
                    target.epoch,
                    &proof_packet(
                        target.width,
                        target.height,
                        target.scale,
                        mode,
                        FrameIdentity::Epoch(target.epoch),
                    ),
                ),
                Some(PendingDraw::Untagged(target, identity)) => queue_packet(
                    target.pane,
                    &proof_packet(target.width, target.height, target.scale, mode, identity),
                ),
                None => Ok(()),
            });
            if let Ok(mut state) = STATE.lock() {
                match (pending, result) {
                    (Some(PendingDraw::Tagged(target)), Ok(())) => {
                        state.last_submitted = Some(target);
                        state.error = None;
                    }
                    (Some(PendingDraw::Untagged(_, _)), Ok(())) | (None, Ok(())) => {
                        state.last_submitted = None;
                        state.error = None;
                    }
                    (_, Err(error)) => {
                        state.pending_draw = pending;
                        state.error = Some(error);
                    }
                }
            }
            Vec::new()
        }
        PresentationEvent::PaneCreated(created) => {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            state.pane = created.pane;
            state.width = created.width.max(1.0);
            state.height = created.height.max(1.0);
            state.scale = created.scale.max(0.01);
            let mode = state.mode;
            let pane = state.pane;
            #[cfg(feature = "pure-renderer")]
            return vec![facet_diagnostic_command(
                "resize-proof-initial",
                mode,
                pane,
                None,
                FrameIdentity::Initial,
                state.width,
                state.height,
                state.scale,
            )];
            #[cfg(not(feature = "pure-renderer"))]
            {
                let (width, height, scale) = (state.width, state.height, state.scale);
                drop(state);
                let result = submit_untagged_frame(pane, width, height, scale, mode);
                record_untagged_result(&result);
                vec![diagnostic_command(
                    "resize-proof-initial",
                    mode,
                    pane,
                    None,
                    width,
                    height,
                    result.is_ok(),
                )]
            }
        }
        #[cfg(not(feature = "pure-renderer"))]
        PresentationEvent::PointerButton(button) => {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            if state.mode == ProofMode::WindowSlow && button.pane == state.pane && button.pressed {
                if button.button == RIGHT_POINTER_BUTTON && state.window_capture_gate.arm() {
                    let pane = state.pane;
                    drop(state);
                    return vec![window_gate_diagnostic(
                        "resize-proof-window-gate-armed",
                        pane,
                        None,
                        None,
                    )];
                }
                if button.button == LEFT_POINTER_BUTTON {
                    let target = state.window_capture_gate.release();
                    drop(state);
                    return target.map_or_else(Vec::new, window_gate_release_commands);
                }
                return Vec::new();
            }
            if state.mode != ProofMode::LeafLayout || button.pane != state.pane || !button.pressed {
                return Vec::new();
            }
            drop(state);
            vec![send_local(
                MIDDLE_LAYOUT_ACTOR_ID,
                r#"{"kind":"arm-resize-region-proof"}"#.to_string(),
            )]
        }
        PresentationEvent::PaneResized(resized) => {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            if resized.pane != state.pane {
                return Vec::new();
            }
            state.width = resized.width.max(1.0);
            state.height = resized.height.max(1.0);
            state.scale = resized.scale.max(0.01);
            let mode = state.mode;
            let Some(epoch) = resized.resize_epoch else {
                let untagged = UntaggedTarget {
                    pane: state.pane,
                    width: state.width,
                    height: state.height,
                    scale: state.scale,
                };
                if mode.gates_untagged_recovery() && state.tagged_observed {
                    let action = state.schedule_gated_recovery(untagged);
                    drop(state);
                    return match action {
                        RecoveryAction::Spawn(worker) => vec![
                            diagnostic_command(
                                "resize-proof-recovery-delayed",
                                mode,
                                untagged.pane,
                                None,
                                untagged.width,
                                untagged.height,
                                true,
                            ),
                            ActorCommand::SpawnWorker(WorkerRequest {
                                worker,
                                payload: RECOVERY_PAYLOAD.to_vec(),
                            }),
                        ],
                        RecoveryAction::Pending => vec![diagnostic_command(
                            "resize-proof-recovery-updated",
                            mode,
                            untagged.pane,
                            None,
                            untagged.width,
                            untagged.height,
                            true,
                        )],
                        RecoveryAction::Immediate => Vec::new(),
                    };
                }
                if mode.stalls() {
                    match state.schedule_first_recovery(untagged) {
                        RecoveryAction::Spawn(worker) => {
                            drop(state);
                            return vec![
                                diagnostic_command(
                                    "resize-proof-recovery-delayed",
                                    mode,
                                    untagged.pane,
                                    None,
                                    untagged.width,
                                    untagged.height,
                                    true,
                                ),
                                ActorCommand::SpawnWorker(WorkerRequest {
                                    worker,
                                    payload: RECOVERY_PAYLOAD.to_vec(),
                                }),
                            ];
                        }
                        RecoveryAction::Pending => {
                            drop(state);
                            return vec![diagnostic_command(
                                "resize-proof-recovery-updated",
                                mode,
                                untagged.pane,
                                None,
                                untagged.width,
                                untagged.height,
                                true,
                            )];
                        }
                        RecoveryAction::Immediate => {}
                    }
                }
                let cancelled = state.delay.reset();
                state.stalled = None;
                let pane = state.pane;
                let (width, height, scale) = (state.width, state.height, state.scale);
                #[cfg(feature = "pure-renderer")]
                {
                    state.frame_identity = FrameIdentity::Initial;
                    state.error = None;
                }
                drop(state);
                #[cfg(not(feature = "pure-renderer"))]
                let result = submit_untagged_frame(pane, width, height, scale, mode);
                #[cfg(not(feature = "pure-renderer"))]
                record_untagged_result(&result);
                let mut commands = cancelled
                    .map(ActorCommand::CancelWorker)
                    .into_iter()
                    .collect::<Vec<_>>();
                #[cfg(feature = "pure-renderer")]
                commands.push(facet_diagnostic_command(
                    "resize-proof-untagged-resize",
                    mode,
                    pane,
                    None,
                    FrameIdentity::Initial,
                    width,
                    height,
                    scale,
                ));
                #[cfg(not(feature = "pure-renderer"))]
                commands.push(diagnostic_command(
                    "resize-proof-untagged-resize",
                    mode,
                    pane,
                    None,
                    width,
                    height,
                    result.is_ok(),
                ));
                return commands;
            };
            let target = PaintTarget {
                pane: state.pane,
                epoch,
                width: state.width,
                height: state.height,
                scale: state.scale,
            };
            state.tagged_observed = true;
            match mode {
                ProofMode::Fast => {
                    drop(state);
                    submit_target(target, mode)
                }
                ProofMode::GatedFast | ProofMode::MiddleLayout | ProofMode::LeafLayout => {
                    let cancelled = state.cancel_recovery();
                    drop(state);
                    let mut commands = cancelled
                        .map(ActorCommand::CancelWorker)
                        .into_iter()
                        .collect::<Vec<_>>();
                    commands.extend(submit_target(target, mode));
                    commands
                }
                ProofMode::Slow => {
                    let action = state.delay.offer(target);
                    drop(state);
                    apply_delay_action(action, mode)
                }
                ProofMode::WindowSlow => {
                    let gate = state.window_capture_gate.offer(target);
                    if gate == WindowGateOffer::NotArmed {
                        let action = state.delay.offer(target);
                        drop(state);
                        return apply_delay_action(action, mode);
                    }
                    let cancelled_worker = state.delay.reset();
                    drop(state);
                    let mut commands = cancelled_worker
                        .map(ActorCommand::CancelWorker)
                        .into_iter()
                        .collect::<Vec<_>>();
                    match gate {
                        WindowGateOffer::Held => commands.push(window_gate_diagnostic(
                            "resize-proof-window-gate-held",
                            target.pane,
                            Some(target),
                            None,
                        )),
                        WindowGateOffer::Replaced { cancelled } => {
                            commands.push(window_gate_diagnostic(
                                "resize-proof-window-gate-replaced",
                                target.pane,
                                Some(target),
                                Some(cancelled.epoch),
                            ));
                        }
                        WindowGateOffer::Stale { held } => {
                            commands.push(window_gate_diagnostic(
                                "resize-proof-window-gate-stale",
                                target.pane,
                                Some(held),
                                Some(target.epoch),
                            ));
                        }
                        WindowGateOffer::NotArmed => {}
                    }
                    commands
                }
                ProofMode::StallOnce | ProofMode::StallAfterFirst => {
                    if state.stall_after_optional_warmup(target) {
                        drop(state);
                        vec![diagnostic_command(
                            "resize-proof-stalled",
                            mode,
                            target.pane,
                            Some(target.epoch),
                            target.width,
                            target.height,
                            true,
                        )]
                    } else {
                        let cancelled = state.cancel_recovery();
                        drop(state);
                        let mut commands = cancelled
                            .map(ActorCommand::CancelWorker)
                            .into_iter()
                            .collect::<Vec<_>>();
                        commands.extend(submit_target(target, mode));
                        commands
                    }
                }
            }
        }
        _ => Vec::new(),
    }
}

fn handle_worker(event: WorkerEvent) -> Vec<ActorCommand> {
    let (worker, failed) = match event {
        WorkerEvent::Finished(finished) => (finished.worker, false),
        WorkerEvent::Failed(failed) => (failed.worker, true),
        WorkerEvent::Cancelled(worker) => (worker, true),
        WorkerEvent::Started(_) => return Vec::new(),
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if let Some(pending) = state.pending_layout_regions.remove(&worker) {
        let mode = state.mode;
        let response_index = worker.saturating_sub(10_000);
        drop(state);
        if failed {
            return vec![layout_region_diagnostic(
                "resize-proof-region-worker-failed",
                mode,
                &pending,
                response_index,
            )];
        }
        return vec![
            layout_region_diagnostic(
                "resize-proof-region-responded",
                mode,
                &pending,
                response_index,
            ),
            layout_region_response(mode, &pending),
        ];
    }
    if state.mode.stalls() || state.mode.gates_untagged_recovery() {
        let recovery = state.finish_recovery(worker);
        let mode = state.mode;
        drop(state);
        return recovery.map_or_else(Vec::new, |target| submit_recovery(target, mode));
    }
    if !matches!(state.mode, ProofMode::Slow | ProofMode::WindowSlow) {
        return Vec::new();
    }
    let action = if failed {
        state.delay.fail(worker)
    } else {
        state.delay.finish(worker)
    };
    let mode = state.mode;
    drop(state);
    apply_delay_action(action, mode)
}

fn apply_delay_action(action: DelayAction, mode: ProofMode) -> Vec<ActorCommand> {
    match action {
        DelayAction::None => Vec::new(),
        DelayAction::Spawn { worker, .. } => {
            let payload = if mode == ProofMode::WindowSlow {
                WINDOW_DELAY_PAYLOAD
            } else {
                DELAY_PAYLOAD
            };
            vec![ActorCommand::SpawnWorker(WorkerRequest {
                worker,
                payload: payload.to_vec(),
            })]
        }
        DelayAction::Draw(target) => submit_target(target, mode),
    }
}

#[cfg(not(feature = "pure-renderer"))]
fn submit_target(target: PaintTarget, mode: ProofMode) -> Vec<ActorCommand> {
    if !brood_plugin_sdk::graphics::render_is_active() {
        let result =
            brood_plugin_sdk::graphics::request_render_for_resize(target.pane, Some(target.epoch));
        if let Ok(mut state) = STATE.lock() {
            state.pending_draw = Some(PendingDraw::Tagged(target));
            if let Err(error) = &result {
                state.error = Some(error.clone());
            }
        }
        return vec![diagnostic_command(
            "resize-proof-tagged",
            mode,
            target.pane,
            Some(target.epoch),
            target.width,
            target.height,
            result.is_ok(),
        )];
    }
    let packet = proof_packet(
        target.width,
        target.height,
        target.scale,
        mode,
        FrameIdentity::Epoch(target.epoch),
    );
    let result = queue_tagged_packet(target.pane, target.epoch, &packet);
    if let Ok(mut state) = STATE.lock() {
        match &result {
            Ok(_sequence) => {
                state.last_submitted = Some(target);
                state.error = None;
            }
            Err(error) => state.error = Some(error.clone()),
        }
    }
    vec![diagnostic_command(
        "resize-proof-tagged",
        mode,
        target.pane,
        Some(target.epoch),
        target.width,
        target.height,
        result.is_ok(),
    )]
}

#[cfg(feature = "pure-renderer")]
fn submit_target(target: PaintTarget, mode: ProofMode) -> Vec<ActorCommand> {
    if let Ok(mut state) = STATE.lock() {
        state.frame_identity = FrameIdentity::Epoch(target.epoch);
        state.error = None;
    }
    vec![facet_diagnostic_command(
        "resize-proof-tagged",
        mode,
        target.pane,
        Some(target.epoch),
        FrameIdentity::Epoch(target.epoch),
        target.width,
        target.height,
        target.scale,
    )]
}

#[cfg(not(feature = "pure-renderer"))]
fn submit_recovery(target: UntaggedTarget, mode: ProofMode) -> Vec<ActorCommand> {
    if !brood_plugin_sdk::graphics::render_is_active() {
        let result = brood_plugin_sdk::graphics::request_render();
        if let Ok(mut state) = STATE.lock() {
            state.pending_draw = Some(PendingDraw::Untagged(target, FrameIdentity::Recovery));
            if let Err(error) = &result {
                state.error = Some(error.clone());
            }
        }
        return vec![diagnostic_command(
            "resize-proof-recovery",
            mode,
            target.pane,
            None,
            target.width,
            target.height,
            result.is_ok(),
        )];
    }
    let result = queue_packet(
        target.pane,
        &proof_packet(
            target.width,
            target.height,
            target.scale,
            mode,
            FrameIdentity::Recovery,
        ),
    );
    record_untagged_result(&result);
    vec![diagnostic_command(
        "resize-proof-recovery",
        mode,
        target.pane,
        None,
        target.width,
        target.height,
        result.is_ok(),
    )]
}

#[cfg(feature = "pure-renderer")]
fn submit_recovery(target: UntaggedTarget, mode: ProofMode) -> Vec<ActorCommand> {
    if let Ok(mut state) = STATE.lock() {
        state.frame_identity = FrameIdentity::Recovery;
        state.error = None;
    }
    vec![facet_diagnostic_command(
        "resize-proof-recovery",
        mode,
        target.pane,
        None,
        FrameIdentity::Recovery,
        target.width,
        target.height,
        target.scale,
    )]
}

#[cfg(not(feature = "pure-renderer"))]
fn submit_untagged_frame(
    pane: u64,
    width: f32,
    height: f32,
    scale: f32,
    mode: ProofMode,
) -> Result<u64, String> {
    if !brood_plugin_sdk::graphics::render_is_active() {
        if let Ok(mut state) = STATE.lock() {
            state.pending_draw = Some(PendingDraw::Untagged(
                UntaggedTarget {
                    pane,
                    width,
                    height,
                    scale,
                },
                FrameIdentity::Initial,
            ));
        }
        brood_plugin_sdk::graphics::request_render()?;
        return Ok(0);
    }
    queue_packet(
        pane,
        &proof_packet(width, height, scale, mode, FrameIdentity::Initial),
    )?;
    Ok(0)
}

#[cfg(not(feature = "pure-renderer"))]
fn record_untagged_result<T>(result: &Result<T, String>) {
    if let Ok(mut state) = STATE.lock() {
        match result {
            Ok(_sequence) => {
                state.last_submitted = None;
                state.error = None;
            }
            Err(error) => state.error = Some(error.clone()),
        }
    }
}

#[cfg(feature = "pure-renderer")]
#[allow(
    clippy::too_many_arguments,
    reason = "diagnostic payload mirrors the resize-proof protocol fields"
)]
fn facet_diagnostic_command(
    kind: &str,
    mode: ProofMode,
    pane: u64,
    epoch: Option<u64>,
    identity: FrameIdentity,
    width: f32,
    height: f32,
    scale: f32,
) -> ActorCommand {
    send_local(
        mode.actor_id(),
        serde_json::to_string(&serde_json::json!({
            "kind": kind,
            "mode": mode.label(),
            "pane": pane,
            "epoch": epoch,
            "identity": identity,
            "width": width,
            "height": height,
            "scale": scale,
            "production": "facet-publication-pending"
        }))
        .unwrap_or_default(),
    )
}

#[cfg(not(feature = "pure-renderer"))]
fn diagnostic_command(
    kind: &str,
    mode: ProofMode,
    pane: u64,
    epoch: Option<u64>,
    width: f32,
    height: f32,
    success: bool,
) -> ActorCommand {
    let epoch = epoch.map_or_else(|| "null".to_string(), |epoch| epoch.to_string());
    send_local(
        mode.actor_id(),
        format!(
            r#"{{"kind":"{kind}","mode":"{}","pane":{pane},"epoch":{epoch},"width":{width:.3},"height":{height:.3},"success":{success}}}"#,
            mode.label()
        ),
    )
}

#[cfg(feature = "pure-renderer")]
fn diagnostic_command(
    kind: &str,
    mode: ProofMode,
    pane: u64,
    epoch: Option<u64>,
    width: f32,
    height: f32,
    _success: bool,
) -> ActorCommand {
    send_local(
        mode.actor_id(),
        serde_json::to_string(&serde_json::json!({
            "kind": kind,
            "mode": mode.label(),
            "pane": pane,
            "epoch": epoch,
            "width": width,
            "height": height,
            "production": "actor-control-updated"
        }))
        .unwrap_or_default(),
    )
}

fn window_gate_diagnostic(
    kind: &str,
    pane: u64,
    held: Option<PaintTarget>,
    cancelled_epoch: Option<u64>,
) -> ActorCommand {
    let epoch = held.map_or(0, |target| target.epoch);
    let held_target = held.map_or_else(
        || "null".to_string(),
        |target| {
            format!(
                r#"{{"pane":{},"epoch":{},"width":{:.3},"height":{:.3},"scale":{:.3}}}"#,
                target.pane, target.epoch, target.width, target.height, target.scale,
            )
        },
    );
    let cancelled_epoch = cancelled_epoch.map_or(0, |epoch| epoch);
    send_local(
        WINDOW_SLOW_ACTOR_ID,
        format!(
            r#"{{"kind":"{kind}","mode":"slow","pane":{pane},"epoch":{epoch},"held_target":{held_target},"cancelled_epoch":{cancelled_epoch}}}"#,
        ),
    )
}

fn window_gate_release_commands(target: PaintTarget) -> Vec<ActorCommand> {
    vec![
        window_gate_diagnostic(
            "resize-proof-window-gate-released",
            target.pane,
            Some(target),
            None,
        ),
        send_local(
            WINDOW_SLOW_ACTOR_ID,
            format!(
                r#"{{"kind":"resize-proof-window-gate-submit","epoch":{}}}"#,
                target.epoch
            ),
        ),
    ]
}

fn handle_window_gate_control(body: &[u8]) -> Vec<ActorCommand> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Vec::new();
    };
    if value.get("kind").and_then(serde_json::Value::as_str)
        != Some("resize-proof-window-gate-submit")
    {
        return Vec::new();
    }
    let Some(epoch) = value.get("epoch").and_then(serde_json::Value::as_u64) else {
        return Vec::new();
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if state.mode != ProofMode::WindowSlow {
        return Vec::new();
    }
    let target = state.window_capture_gate.take_pending_release(epoch);
    drop(state);
    target.map_or_else(Vec::new, |target| {
        submit_target(target, ProofMode::WindowSlow)
    })
}

fn proof_packet(
    width: f32,
    height: f32,
    scale: f32,
    mode: ProofMode,
    identity: FrameIdentity,
) -> Packet {
    let width = width.max(1.0);
    let height = height.max(1.0);
    let edge = width.min(height).mul_add(0.018, 0.0).clamp(3.0, 8.0);
    let barcode_height = (height * 0.055).clamp(8.0, 18.0);
    let (background, identity_bits) = match identity {
        FrameIdentity::Initial => (INITIAL_BACKGROUND, 0),
        FrameIdentity::Recovery => (RECOVERY_BACKGROUND, u64::MAX),
        FrameIdentity::Epoch(epoch) => (epoch_background(epoch), epoch),
    };
    let mut packet = Packet::new();
    push_rect(&mut packet, 0.0, 0.0, width, height, background);

    push_rect(&mut packet, 0.0, 0.0, width, edge, BIT_ONE);
    push_rect(&mut packet, 0.0, height - edge, width, edge, BIT_ONE);
    push_rect(&mut packet, 0.0, 0.0, edge, height, BIT_ONE);
    push_rect(&mut packet, width - edge, 0.0, edge, height, BIT_ONE);

    let inner_width = (width - edge * 2.0).max(1.0);
    push_horizontal_bits(
        &mut packet,
        edge,
        barcode_height,
        edge,
        inner_width,
        identity_bits,
    );
    let size_bits = u64::from(width.to_bits()) | (u64::from(height.to_bits()) << 32);
    push_horizontal_bits(
        &mut packet,
        (height - edge - barcode_height).max(edge),
        barcode_height,
        edge,
        inner_width,
        size_bits,
    );
    push_vertical_bits(
        &mut packet,
        (width - edge - barcode_height).max(edge),
        edge + barcode_height,
        barcode_height,
        (height - (edge + barcode_height) * 2.0).max(1.0),
        scale.to_bits(),
    );

    let marker = mode.marker();
    push_rect(
        &mut packet,
        edge,
        edge + barcode_height,
        barcode_height,
        (height - (edge + barcode_height) * 2.0).max(1.0),
        marker,
    );
    push_rect(
        &mut packet,
        width * 0.5 - 1.5,
        edge + barcode_height,
        3.0,
        (height - (edge + barcode_height) * 2.0).max(1.0),
        marker,
    );
    push_rect(
        &mut packet,
        edge + barcode_height,
        height * 0.5 - 1.5,
        (width - (edge + barcode_height) * 2.0).max(1.0),
        3.0,
        marker,
    );
    packet
}

fn push_horizontal_bits(packet: &mut Packet, y: f32, height: f32, x: f32, width: f32, bits: u64) {
    let cell_width = width / 64.0;
    for index in 0_u32..64 {
        let color = if bits & (1_u64 << index) == 0 {
            BIT_ZERO
        } else {
            BIT_ONE
        };
        let offset = f32::from(u16::try_from(index).unwrap_or_default()) * cell_width;
        push_rect(packet, x + offset, y, cell_width + 0.25, height, color);
    }
}

fn push_vertical_bits(packet: &mut Packet, x: f32, y: f32, width: f32, height: f32, bits: u32) {
    let cell_height = height / 32.0;
    for index in 0_u32..32 {
        let color = if bits & (1_u32 << index) == 0 {
            BIT_ZERO
        } else {
            BIT_ONE
        };
        let offset = f32::from(u16::try_from(index).unwrap_or_default()) * cell_height;
        push_rect(packet, x, y + offset, width, cell_height + 0.25, color);
    }
}

fn push_rect(packet: &mut Packet, x: f32, y: f32, width: f32, height: f32, color: Color) {
    packet.push(Command::FillRect {
        rect: Rect::new(x, y, width.max(0.0), height.max(0.0)),
        color,
    });
}

fn epoch_background(epoch: u64) -> Color {
    Color::rgba(
        epoch_channel(epoch, 0),
        epoch_channel(epoch, 2),
        epoch_channel(epoch, 4),
        1.0,
    )
}

fn epoch_channel(epoch: u64, shift: u32) -> f32 {
    let bits = u8::try_from((epoch >> shift) & 0b11).unwrap_or_default();
    f32::from(bits).mul_add(0.20, 0.22)
}

#[cfg(feature = "pure-renderer")]
fn handle_facet_input(events: Vec<Vec<u8>>) -> Vec<ActorCommand> {
    let mut commands = Vec::new();
    for bytes in events {
        let Ok(intent) = serde_json::from_slice::<FacetIntent>(&bytes) else {
            continue;
        };
        let (pane, mode, identity) = match intent {
            FacetIntent::ArmWindowGate {
                pane,
                mode,
                identity,
            }
            | FacetIntent::ReleaseWindowGate {
                pane,
                mode,
                identity,
            }
            | FacetIntent::ArmRegionProof {
                pane,
                mode,
                identity,
            } => (pane, mode, identity),
        };
        let Ok(mut state) = STATE.lock() else {
            continue;
        };
        if pane != state.pane || mode != state.mode {
            continue;
        }
        match intent {
            FacetIntent::ArmWindowGate { .. } if mode == ProofMode::WindowSlow => {
                if state.window_capture_gate.arm() {
                    commands.push(facet_control_diagnostic(
                        "resize-proof-window-gate-armed",
                        mode,
                        pane,
                        identity,
                    ));
                }
            }
            FacetIntent::ReleaseWindowGate { .. } if mode == ProofMode::WindowSlow => {
                let target = state.window_capture_gate.release();
                drop(state);
                commands.extend(target.map_or_else(Vec::new, window_gate_release_commands));
                continue;
            }
            FacetIntent::ArmRegionProof { .. } if mode == ProofMode::LeafLayout => {
                drop(state);
                commands.push(send_local(
                    MIDDLE_LAYOUT_ACTOR_ID,
                    r#"{"kind":"arm-resize-region-proof"}"#.to_string(),
                ));
                continue;
            }
            _ => {}
        }
    }
    commands
}

#[cfg(feature = "pure-renderer")]
fn facet_control_diagnostic(
    kind: &str,
    mode: ProofMode,
    pane: u64,
    identity: FrameIdentity,
) -> ActorCommand {
    send_local(
        mode.actor_id(),
        serde_json::to_string(&serde_json::json!({
            "kind": kind,
            "pane": pane,
            "source_frame_identity": identity
        }))
        .unwrap_or_default(),
    )
}

#[cfg(feature = "pure-renderer")]
fn facet_decode_limits(len: usize) -> facet_codec::DecodeLimits {
    facet_codec::DecodeLimits {
        max_wire_bytes: len,
        max_allocated_bytes: len.saturating_mul(4).max(4096),
        max_allocations: 4096,
    }
}

#[cfg(feature = "pure-renderer")]
impl Facets for ResizeProof {
    fn render(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_render_request(&input, facet_decode_limits(input.len()))
            .expect("valid resize proof render request");
        let view: RenderView = serde_json::from_slice(&request.application_view)
            .expect("valid immutable resize proof view");
        let panes = request
            .panes
            .iter()
            .map(|pane| facets::PaneOutput {
                pane: pane.pane,
                content: facets::PaneContent::Draw {
                    packet: proof_packet(
                        pane.logical_width as f32,
                        pane.logical_height as f32,
                        pane.scale as f32,
                        view.mode,
                        view.identity,
                    )
                    .encode()
                    .expect("valid resize proof packet"),
                    interaction: serde_json::to_vec(&InteractionMap {
                        pane: pane.pane,
                        mode: view.mode,
                        identity: view.identity,
                    })
                    .expect("resize proof interaction map"),
                },
            })
            .collect();
        facet_codec::encode_render_response_owned(
            &facets::RenderResponse {
                token: request.token,
                panes,
                next_frame_ns: None,
            },
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded resize proof render response")
    }

    fn interpret(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_input_request(&input, facet_decode_limits(input.len()))
            .expect("valid resize proof input request");
        let map: InteractionMap = serde_json::from_slice(&request.interaction)
            .expect("valid resize proof interaction map");
        let intent = match request.event {
            facets::InputEvent::PointerButton {
                button: RIGHT_POINTER_BUTTON,
                pressed: true,
                ..
            } if map.mode == ProofMode::WindowSlow => Some(FacetIntent::ArmWindowGate {
                pane: map.pane,
                mode: map.mode,
                identity: map.identity,
            }),
            facets::InputEvent::PointerButton {
                button: LEFT_POINTER_BUTTON,
                pressed: true,
                ..
            } if map.mode == ProofMode::WindowSlow => Some(FacetIntent::ReleaseWindowGate {
                pane: map.pane,
                mode: map.mode,
                identity: map.identity,
            }),
            facets::InputEvent::PointerButton { pressed: true, .. }
                if map.mode == ProofMode::LeafLayout =>
            {
                Some(FacetIntent::ArmRegionProof {
                    pane: map.pane,
                    mode: map.mode,
                    identity: map.identity,
                })
            }
            _ => None,
        };
        let events = intent
            .map(|intent| serde_json::to_vec(&intent).expect("resize proof semantic intent"))
            .into_iter()
            .collect();
        facet_codec::encode_input_response_owned(
            &facets::InputResponse {
                token: request.token,
                next_sequence_state: request.sequence_state,
                events,
                requests: Vec::new(),
            },
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded resize proof input response")
    }
}

#[cfg(feature = "pure-renderer")]
export_visual_plugin!(ResizeProof);
#[cfg(not(feature = "pure-renderer"))]
bindings::export!(ResizeProof with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    const fn target(epoch: u64, width: f32) -> PaintTarget {
        PaintTarget {
            pane: 7,
            epoch,
            width,
            height: 240.0,
            scale: 1.0,
        }
    }

    #[test]
    fn instance_suffix_selects_fast_slow_and_stall_modes() {
        assert_eq!(ProofMode::for_instance(FAST_ACTOR_ID), ProofMode::Fast);
        assert_eq!(
            ProofMode::for_instance(GATED_FAST_ACTOR_ID),
            ProofMode::GatedFast
        );
        assert_eq!(ProofMode::for_instance(SLOW_ACTOR_ID), ProofMode::Slow);
        assert_eq!(
            ProofMode::for_instance(WINDOW_SLOW_ACTOR_ID),
            ProofMode::WindowSlow
        );
        assert_eq!(
            ProofMode::for_instance(STALL_ACTOR_ID),
            ProofMode::StallOnce
        );
        assert_eq!(
            ProofMode::for_instance(STALL_AFTER_FIRST_ACTOR_ID),
            ProofMode::StallAfterFirst
        );
        assert_eq!(
            ProofMode::for_instance(MIDDLE_LAYOUT_ACTOR_ID),
            ProofMode::MiddleLayout
        );
        assert_eq!(
            ProofMode::for_instance(LEAF_LAYOUT_ACTOR_ID),
            ProofMode::LeafLayout
        );
        assert_eq!(ProofMode::for_instance("resize-proof"), ProofMode::Fast);
    }

    #[test]
    fn region_delay_payload_round_trips_each_layout_role() {
        for (mode, delay) in [(ProofMode::MiddleLayout, 150), (ProofMode::LeafLayout, 30)] {
            assert_eq!(
                region_delay_ms(&region_delay_payload(mode, delay)),
                Some(delay)
            );
        }
        assert_eq!(region_delay_ms(b"resize-proof-delay-v1"), None);
    }

    #[test]
    fn middle_fragment_has_one_direct_pane_and_one_explicit_rest_slot() {
        let (layout, rest) = proof_layout_fragment(ProofMode::MiddleLayout, 17, true);
        assert_eq!(layout["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(layout["items"][0]["kind"]["pane"], 17);
        assert_eq!(layout["items"][0]["transform"]["size"]["u"], 0.8);
        assert_eq!(rest["transform"]["offset"]["u"], -0.2);
        assert_eq!(rest["transform"]["size"]["u"], 1.2);

        let (layout, rest) = proof_layout_fragment(ProofMode::LeafLayout, 19, false);
        assert_eq!(layout["items"][0]["kind"]["pane"], 19);
        assert_eq!(layout["items"][0]["transform"]["size"]["u"], 2.0);
        assert!(rest.is_null());
    }

    #[test]
    fn middle_delegates_only_non_owned_panes_before_publishing_rest() {
        let context = ProofLayoutContext {
            revision: 9,
            presented_layout_revision: 3,
            lease: ProofLayoutLease {
                id: 4,
                generation: 2,
            },
            panes: vec![17, 19],
            size: ProofLayoutSize { w: 800, h: 600 },
            rest: false,
        };
        let commands = publish_proof_layout(ProofMode::MiddleLayout, 17, &context);
        let [ActorCommand::SendHostMessage(body)] = commands.as_slice() else {
            panic!("middle did not publish one delegation command");
        };
        let value: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(value["type"], "delegate-layout-rest");
        assert_eq!(value["actor"], LEAF_LAYOUT_ACTOR_ID);
        assert_eq!(value["panes"], serde_json::json!([19]));
    }

    #[test]
    fn region_response_echoes_exact_epoch_lease_and_pixel_extent_diagnostic() {
        let pending = PendingLayoutRegion {
            epoch: ProofResizeEpoch {
                id: 31,
                lease: 1,
                lease_generation: 2,
                base_layout_revision: 7,
                target_layout_revision: 8,
            },
            lease: ProofLayoutLease {
                id: 4,
                generation: 3,
            },
            size: ProofLayoutSize { w: 650, h: 480 },
            scale: 1.25,
            revision: 9,
            pane: 17,
            has_rest: true,
        };
        let ActorCommand::SendHostMessage(body) =
            layout_region_response(ProofMode::MiddleLayout, &pending)
        else {
            panic!("region response did not use the host mailbox");
        };
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["type"], "submit-layout-region");
        assert_eq!(value["resize_epoch"]["id"], 31);
        assert_eq!(
            value["lease"],
            serde_json::json!({"id": 4, "generation": 3})
        );
        assert_eq!(value["inventory_revision"], 9);
        assert!(value["fragment"]["rest"].is_object());
    }

    #[test]
    fn burst_coalesces_b_through_i_and_only_draws_i_after_two_cycles() {
        let mut scheduler = DelayScheduler::new();
        let targets = (11..=19)
            .map(|epoch| target(epoch, 320.0 + (epoch - 11) as f32 * 20.0))
            .collect::<Vec<_>>();
        let first = targets[0];
        let final_target = targets[8];
        let first_worker = match scheduler.offer(first) {
            DelayAction::Spawn { worker, target } if target == first => Some(worker),
            _ => None,
        };
        assert!(first_worker.is_some());
        for replacement in &targets[1..] {
            assert_eq!(scheduler.offer(*replacement), DelayAction::None);
        }
        assert_eq!(scheduler.pending, Some(final_target));
        let Some(first_worker) = first_worker else {
            return;
        };
        let final_worker = match scheduler.finish(first_worker) {
            DelayAction::Spawn { worker, target } if target == final_target => Some(worker),
            _ => None,
        };
        assert!(final_worker.is_some());
        let Some(final_worker) = final_worker else {
            return;
        };
        assert_eq!(
            scheduler.finish(final_worker),
            DelayAction::Draw(final_target)
        );
        assert_eq!(scheduler.pending, None);
        assert_eq!(scheduler.active, None);
        assert_eq!(scheduler.finish(final_worker), DelayAction::None);
    }

    #[test]
    fn final_target_draws_once_after_its_delay() {
        let mut scheduler = DelayScheduler::new();
        let final_target = target(99, 640.0);
        let worker = match scheduler.offer(final_target) {
            DelayAction::Spawn { worker, .. } => Some(worker),
            _ => None,
        };
        assert!(worker.is_some());
        let Some(worker) = worker else {
            return;
        };
        assert_eq!(scheduler.finish(worker), DelayAction::Draw(final_target));
        assert_eq!(scheduler.finish(worker), DelayAction::None);
    }

    #[test]
    fn window_capture_gate_arms_holds_and_releases_one_target() {
        let mut gate = WindowCaptureGate::new();
        let held = target(21, 520.0);
        assert!(gate.arm());
        assert!(!gate.arm());
        assert_eq!(gate.offer(held), WindowGateOffer::Held);
        assert_eq!(gate.release(), Some(held));
    }

    #[test]
    fn window_capture_gate_supersession_replaces_the_held_target() {
        let mut gate = WindowCaptureGate::new();
        let obsolete = target(21, 520.0);
        let current = target(22, 560.0);
        assert!(gate.arm());
        assert_eq!(gate.offer(obsolete), WindowGateOffer::Held);
        assert_eq!(
            gate.offer(current),
            WindowGateOffer::Replaced {
                cancelled: obsolete
            }
        );
        assert_eq!(gate.release(), Some(current));
    }

    #[test]
    fn window_capture_gate_duplicate_release_submits_nothing() {
        let mut gate = WindowCaptureGate::new();
        let held = target(21, 520.0);
        assert!(gate.arm());
        assert_eq!(gate.offer(held), WindowGateOffer::Held);
        assert_eq!(gate.release(), Some(held));
        assert_eq!(gate.release(), None);
    }

    #[test]
    fn window_capture_gate_stale_epoch_cannot_replace_the_current_target() {
        let mut gate = WindowCaptureGate::new();
        let stale = target(21, 520.0);
        let current = target(22, 560.0);
        assert!(gate.arm());
        assert_eq!(gate.offer(current), WindowGateOffer::Held);
        assert_eq!(gate.offer(stale), WindowGateOffer::Stale { held: current });
        assert_eq!(gate.release(), Some(current));
    }

    #[test]
    fn window_capture_gate_release_commands_log_before_exact_epoch_submit() {
        let held = target(22, 560.0);
        let commands = window_gate_release_commands(held);
        let [
            ActorCommand::SendLocalMessage(released),
            ActorCommand::SendLocalMessage(submit),
        ] = commands.as_slice()
        else {
            panic!("release did not produce two ordered local messages");
        };
        assert_eq!(released.actor, WINDOW_SLOW_ACTOR_ID);
        assert_eq!(submit.actor, WINDOW_SLOW_ACTOR_ID);
        let released: serde_json::Value = serde_json::from_slice(&released.body).unwrap();
        let submit: serde_json::Value = serde_json::from_slice(&submit.body).unwrap();
        assert_eq!(released["kind"], "resize-proof-window-gate-released");
        assert_eq!(released["held_target"]["epoch"], 22);
        assert_eq!(submit["kind"], "resize-proof-window-gate-submit");
        assert_eq!(submit["epoch"], 22);
    }

    #[test]
    fn window_capture_gate_submit_requires_exact_epoch_and_consumes_once() {
        let mut gate = WindowCaptureGate::new();
        let held = target(22, 560.0);
        assert!(gate.arm());
        assert_eq!(gate.offer(held), WindowGateOffer::Held);
        assert_eq!(gate.release(), Some(held));
        assert_eq!(gate.take_pending_release(21), None);
        assert_eq!(gate.take_pending_release(22), Some(held));
        assert_eq!(gate.take_pending_release(22), None);
    }

    #[test]
    fn window_capture_gate_supersession_invalidates_stale_submit_message() {
        let mut gate = WindowCaptureGate::new();
        let obsolete = target(21, 520.0);
        let current = target(22, 560.0);
        assert!(gate.arm());
        assert_eq!(gate.offer(obsolete), WindowGateOffer::Held);
        assert_eq!(
            gate.offer(current),
            WindowGateOffer::Replaced {
                cancelled: obsolete
            }
        );
        assert_eq!(gate.release(), Some(current));
        assert_eq!(gate.take_pending_release(21), None);
        assert_eq!(gate.take_pending_release(22), Some(current));
    }

    #[test]
    fn newer_target_cancels_a_pending_old_release() {
        let mut gate = WindowCaptureGate::new();
        let obsolete = target(21, 520.0);
        let current = target(22, 560.0);
        assert!(gate.arm());
        assert_eq!(gate.offer(obsolete), WindowGateOffer::Held);
        assert_eq!(gate.release(), Some(obsolete));
        assert_eq!(gate.offer(current), WindowGateOffer::NotArmed);
        assert_eq!(gate.take_pending_release(21), None);
    }

    #[test]
    fn stall_mode_withholds_every_epoch_until_untagged_recovery() {
        let mut state = ProofState::new(ProofMode::StallOnce);
        let first = target(101, 400.0);
        let repeated_first = target(101, 420.0);
        let superseding = target(102, 440.0);
        assert!(state.stall_until_recovery(first));
        assert!(state.stall_until_recovery(repeated_first));
        assert!(state.stall_until_recovery(superseding));

        let untagged = UntaggedTarget {
            pane: 7,
            width: 440.0,
            height: 240.0,
            scale: 1.0,
        };
        assert_eq!(
            state.schedule_first_recovery(untagged),
            RecoveryAction::Spawn(RECOVERY_WORKER)
        );
        assert!(!state.stall_until_recovery(target(103, 460.0)));
    }

    #[test]
    fn stall_after_first_tags_native_warmup_then_stalls_until_recovery() {
        let mut state = ProofState::new(ProofMode::StallAfterFirst);
        let native = target(1, 400.0);
        let pointer = target(2, 420.0);
        let superseding_pointer = target(3, 440.0);

        assert!(!state.stall_after_optional_warmup(native));
        assert!(state.stall_after_optional_warmup(pointer));
        assert!(state.stall_after_optional_warmup(superseding_pointer));

        let untagged = UntaggedTarget {
            pane: 7,
            width: 440.0,
            height: 240.0,
            scale: 1.0,
        };
        assert_eq!(
            state.schedule_first_recovery(untagged),
            RecoveryAction::Spawn(RECOVERY_WORKER)
        );
        assert!(!state.stall_after_optional_warmup(target(4, 460.0)));
    }

    #[test]
    fn stalled_recovery_delay_keeps_the_newest_untagged_target() {
        let mut state = ProofState::new(ProofMode::StallOnce);
        assert!(state.stall_until_recovery(target(101, 400.0)));
        let first = UntaggedTarget {
            pane: 7,
            width: 400.0,
            height: 240.0,
            scale: 1.0,
        };
        let final_target = UntaggedTarget {
            width: 420.0,
            ..first
        };
        assert_eq!(
            state.schedule_first_recovery(first),
            RecoveryAction::Spawn(RECOVERY_WORKER)
        );
        assert_eq!(
            state.schedule_first_recovery(final_target),
            RecoveryAction::Pending
        );
        assert_eq!(state.finish_recovery(RECOVERY_WORKER), Some(final_target));
    }

    #[test]
    fn tagged_recovery_cancels_the_delayed_untagged_frame() {
        let mut state = ProofState::new(ProofMode::StallAfterFirst);
        assert!(!state.stall_after_optional_warmup(target(1, 400.0)));
        assert!(state.stall_after_optional_warmup(target(2, 420.0)));
        let untagged = UntaggedTarget {
            pane: 7,
            width: 420.0,
            height: 240.0,
            scale: 1.0,
        };
        assert_eq!(
            state.schedule_first_recovery(untagged),
            RecoveryAction::Spawn(RECOVERY_WORKER)
        );

        assert!(!state.stall_after_optional_warmup(target(3, 440.0)));
        assert_eq!(state.cancel_recovery(), Some(RECOVERY_WORKER));
        assert_eq!(state.finish_recovery(RECOVERY_WORKER), None);
    }

    #[test]
    fn tagged_epoch_cancels_the_fast_panes_delayed_ordinary_recovery() {
        let mut state = ProofState::new(ProofMode::GatedFast);
        state.tagged_observed = true;
        let untagged = UntaggedTarget {
            pane: 7,
            width: 420.0,
            height: 240.0,
            scale: 1.0,
        };
        assert_eq!(
            state.schedule_gated_recovery(untagged),
            RecoveryAction::Spawn(RECOVERY_WORKER)
        );

        assert_eq!(state.cancel_recovery(), Some(RECOVERY_WORKER));
        assert_eq!(state.finish_recovery(RECOVERY_WORKER), None);
    }

    #[test]
    fn frame_fiducials_change_for_epoch_and_keep_exact_extent_markers() {
        let initial = proof_packet(320.0, 240.0, 1.0, ProofMode::Fast, FrameIdentity::Initial);
        let tagged = proof_packet(480.0, 300.0, 1.25, ProofMode::Fast, FrameIdentity::Epoch(7));
        assert_ne!(initial.commands, tagged.commands);
        assert_eq!(initial.commands.len(), tagged.commands.len());
        assert!(initial.commands.len() > 160);
    }

    #[cfg(feature = "pure-renderer")]
    fn facet_limits() -> facets::InvocationLimits {
        facets::InvocationLimits {
            max_output_bytes: 128 * 1024,
            max_packet_bytes: 64 * 1024,
            max_interaction_bytes: 1024,
            max_sequence_bytes: 1024,
            max_events: 4,
            max_event_bytes: 1024,
            max_host_requests: 2,
            max_resources: 0,
            max_resource_bytes: 0,
        }
    }

    #[cfg(feature = "pure-renderer")]
    fn facet_scope() -> facets::Scope {
        facets::Scope {
            client_generation: 1,
            actor_generation: 2,
            facet_generation: 3,
        }
    }

    #[cfg(feature = "pure-renderer")]
    fn facet_pane(pane: u64, width: f64, height: f64, scale: f64) -> facets::PaneTarget {
        facets::PaneTarget {
            pane,
            logical_width: width,
            logical_height: height,
            physical_width: (width * scale) as u32,
            physical_height: (height * scale) as u32,
            scale,
            clip: [0.0, 0.0, width, height],
            visible: true,
            focused: pane == 7,
        }
    }

    #[cfg(feature = "pure-renderer")]
    #[test]
    fn facet_retry_preserves_exact_pane_set_geometry_scale_and_identity() {
        let token = facets::RenderToken {
            scope: facet_scope(),
            invocation: 4,
            snapshot_revision: 5,
            candidate: 6,
            pane_membership_revision: 7,
            geometry_revision: 8,
            host_configuration_revision: 9,
            renderer_generation: 10,
            interaction_revision: 11,
        };
        let targets = vec![
            facet_pane(7, 333.0, 217.0, 1.25),
            facet_pane(9, 481.0, 302.0, 2.0),
        ];
        let request = facets::RenderRequest {
            token: token.clone(),
            panes: targets.clone(),
            target_time_ns: 123,
            cutoff_ns: 456,
            configuration: Vec::new(),
            application_view: serde_json::to_vec(&RenderView {
                mode: ProofMode::Slow,
                identity: FrameIdentity::Epoch(77),
            })
            .unwrap(),
            sequence_state: vec![4, 5],
            limits: facet_limits(),
        };
        let mut wire = vec![0; 4096];
        let len = facet_codec::encode_render_request(&request, &mut wire).unwrap();
        wire.truncate(len);
        let first = ResizeProof::render(wire.clone());
        assert_eq!(first, ResizeProof::render(wire));
        let reply =
            facet_codec::decode_render_response(&first, facet_decode_limits(first.len())).unwrap();
        assert_eq!(reply.token, token);
        assert_eq!(reply.panes.len(), 2);
        for (output, target) in reply.panes.iter().zip(targets) {
            assert_eq!(output.pane, target.pane);
            let facets::PaneContent::Draw {
                packet,
                interaction,
            } = &output.content
            else {
                panic!("resize proof must draw every requested pane")
            };
            assert_eq!(
                *packet,
                proof_packet(
                    target.logical_width as f32,
                    target.logical_height as f32,
                    target.scale as f32,
                    ProofMode::Slow,
                    FrameIdentity::Epoch(77),
                )
                .encode()
                .unwrap()
            );
            let map: InteractionMap = serde_json::from_slice(interaction).unwrap();
            assert_eq!(map.pane, target.pane);
            assert_eq!(map.identity, FrameIdentity::Epoch(77));
        }
    }

    #[cfg(feature = "pure-renderer")]
    #[test]
    fn facet_pointer_intent_is_bound_to_pinned_proof_identity_and_preserves_sequence() {
        let token = facets::InputToken {
            scope: facet_scope(),
            input_sequence: 12,
            frame: 13,
            pane: 7,
            snapshot_revision: 14,
            sequence_revision: 15,
        };
        let request = facets::InputRequest {
            token: token.clone(),
            geometry: facet_pane(7, 400.0, 300.0, 1.0),
            event: facets::InputEvent::PointerButton {
                x: 20.0,
                y: 30.0,
                button: RIGHT_POINTER_BUTTON,
                pressed: true,
                modifiers: 0,
            },
            interaction: serde_json::to_vec(&InteractionMap {
                pane: 7,
                mode: ProofMode::WindowSlow,
                identity: FrameIdentity::Epoch(91),
            })
            .unwrap(),
            sequence_state: vec![8, 9, 10],
            limits: facet_limits(),
        };
        let mut wire = vec![0; 4096];
        let len = facet_codec::encode_input_request(&request, &mut wire).unwrap();
        wire.truncate(len);
        let first = ResizeProof::interpret(wire.clone());
        assert_eq!(first, ResizeProof::interpret(wire));
        let reply =
            facet_codec::decode_input_response(&first, facet_decode_limits(first.len())).unwrap();
        assert_eq!(reply.token, token);
        assert_eq!(reply.next_sequence_state, vec![8, 9, 10]);
        assert!(reply.requests.is_empty());
        assert_eq!(
            serde_json::from_slice::<FacetIntent>(&reply.events[0]).unwrap(),
            FacetIntent::ArmWindowGate {
                pane: 7,
                mode: ProofMode::WindowSlow,
                identity: FrameIdentity::Epoch(91),
            }
        );
    }

    #[cfg(feature = "pure-renderer")]
    #[test]
    fn facet_readiness_diagnostic_does_not_claim_host_commit() {
        let ActorCommand::SendLocalMessage(message) = facet_diagnostic_command(
            "resize-proof-tagged",
            ProofMode::Fast,
            7,
            Some(44),
            FrameIdentity::Epoch(44),
            640.0,
            480.0,
            1.25,
        ) else {
            panic!("facet diagnostic must remain actor-local")
        };
        let value: serde_json::Value = serde_json::from_slice(&message.body).unwrap();
        assert_eq!(value["production"], "facet-publication-pending");
        assert!(value.get("success").is_none());
        assert!(value.get("committed").is_none());
    }
}
