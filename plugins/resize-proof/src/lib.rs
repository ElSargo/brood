use std::sync::Mutex;

use brood_graphics::{Color, Command, Packet, Rect};
use brood_plugin_sdk::{
    actor::{send_local, start_global_actor},
    bindings,
    exports::brood::plugin::actor::{
        Command as ActorCommand, Event, Guest, LifecycleEvent, PresentationEvent, WorkerEvent,
        WorkerRequest,
    },
    graphics::{submit_packet, submit_tagged_packet},
    runtime::sleep,
};

const INIT_ACTOR_ID: &str = "brood.client-init";
const FAST_ACTOR_ID: &str = "resize-proof-fast";
const SLOW_ACTOR_ID: &str = "resize-proof-slow";
const STALL_ACTOR_ID: &str = "resize-proof-stall";
const STALL_AFTER_FIRST_ACTOR_ID: &str = "resize-proof-stall-after-first";
const COMPONENT_FILE: &str = "brood_resize_proof_plugin.component.wasm";
const DELAY_MS: u64 = 120;
const DELAY_PAYLOAD: &[u8] = b"resize-proof-delay-v1";
const RECOVERY_PAYLOAD: &[u8] = b"resize-proof-recovery-v1";
const RECOVERY_WORKER: u64 = 1;
const INITIAL_BACKGROUND: Color = Color::rgba(0.04, 0.08, 0.18, 1.0);
const RECOVERY_BACKGROUND: Color = Color::rgba(0.08, 0.36, 0.16, 1.0);
const BIT_ZERO: Color = Color::rgba(0.02, 0.02, 0.025, 1.0);
const BIT_ONE: Color = Color::rgba(0.96, 0.97, 0.99, 1.0);
const FAST_MARKER: Color = Color::rgba(0.04, 0.82, 0.92, 1.0);
const SLOW_MARKER: Color = Color::rgba(0.92, 0.12, 0.72, 1.0);
const STALL_MARKER: Color = Color::rgba(0.96, 0.58, 0.08, 1.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProofMode {
    Fast,
    Slow,
    StallOnce,
    StallAfterFirst,
}

impl ProofMode {
    fn for_instance(instance_id: &str) -> Self {
        if instance_id.ends_with("-slow") {
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
            Self::Fast => "fast",
            Self::Slow => "slow",
            Self::StallOnce => "stall-once",
            Self::StallAfterFirst => "stall-after-first",
        }
    }

    const fn actor_id(self) -> &'static str {
        match self {
            Self::Fast => FAST_ACTOR_ID,
            Self::Slow => SLOW_ACTOR_ID,
            Self::StallOnce => STALL_ACTOR_ID,
            Self::StallAfterFirst => STALL_AFTER_FIRST_ACTOR_ID,
        }
    }

    const fn marker(self) -> Color {
        match self {
            Self::Fast => FAST_MARKER,
            Self::Slow => SLOW_MARKER,
            Self::StallOnce | Self::StallAfterFirst => STALL_MARKER,
        }
    }

    const fn stalls(self) -> bool {
        matches!(self, Self::StallOnce | Self::StallAfterFirst)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameIdentity {
    Initial,
    Recovery,
    Epoch(u64),
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

struct ProofState {
    mode: ProofMode,
    pane: u64,
    width: f32,
    height: f32,
    scale: f32,
    delay: DelayScheduler,
    stalled: Option<PaintTarget>,
    recovery_observed: bool,
    recovery_pending: Option<UntaggedTarget>,
    recovery_worker: Option<u64>,
    recovery_delayed: bool,
    first_tagged_submitted: bool,
    last_submitted: Option<PaintTarget>,
    error: Option<String>,
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
            stalled: None,
            recovery_observed: false,
            recovery_pending: None,
            recovery_worker: None,
            recovery_delayed: false,
            first_tagged_submitted: false,
            last_submitted: None,
            error: None,
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

struct ResizeProof;

impl Guest for ResizeProof {
    fn dispatch(event: Event) -> Vec<ActorCommand> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(context)) => handle_start(&context.instance_id),
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Presentation(event) => handle_presentation(event),
            Event::Worker(event) => handle_worker(event),
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        if payload != DELAY_PAYLOAD && payload != RECOVERY_PAYLOAD {
            return Err("resize proof worker received an unknown payload".to_string());
        }
        sleep(DELAY_MS)
            .await
            .map_err(|error| format!("resize proof delay: {error}"))?;
        Ok(payload)
    }
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

fn handle_presentation(event: PresentationEvent) -> Vec<ActorCommand> {
    match event {
        PresentationEvent::PaneCreated(created) => {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            state.pane = created.pane;
            state.width = created.width.max(1.0);
            state.height = created.height.max(1.0);
            state.scale = created.scale.max(0.01);
            let mode = state.mode;
            let (pane, width, height, scale) = (state.pane, state.width, state.height, state.scale);
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
                let (pane, width, height, scale) =
                    (state.pane, state.width, state.height, state.scale);
                drop(state);
                let result = submit_untagged_frame(pane, width, height, scale, mode);
                record_untagged_result(&result);
                let mut commands = cancelled
                    .map(ActorCommand::CancelWorker)
                    .into_iter()
                    .collect::<Vec<_>>();
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
            match mode {
                ProofMode::Fast => {
                    drop(state);
                    submit_target(target, mode)
                }
                ProofMode::Slow => {
                    let action = state.delay.offer(target);
                    drop(state);
                    apply_delay_action(action, mode)
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
    if state.mode.stalls() {
        let recovery = state.finish_recovery(worker);
        let mode = state.mode;
        drop(state);
        return recovery.map_or_else(Vec::new, |target| submit_recovery(target, mode));
    }
    if state.mode != ProofMode::Slow {
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
        DelayAction::Spawn { worker, .. } => vec![ActorCommand::SpawnWorker(WorkerRequest {
            worker,
            payload: DELAY_PAYLOAD.to_vec(),
        })],
        DelayAction::Draw(target) => submit_target(target, mode),
    }
}

fn submit_target(target: PaintTarget, mode: ProofMode) -> Vec<ActorCommand> {
    let packet = proof_packet(
        target.width,
        target.height,
        target.scale,
        mode,
        FrameIdentity::Epoch(target.epoch),
    );
    let result = submit_tagged_packet(target.pane, target.epoch, &packet);
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

fn submit_recovery(target: UntaggedTarget, mode: ProofMode) -> Vec<ActorCommand> {
    let result = submit_packet(
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

fn submit_untagged_frame(
    pane: u64,
    width: f32,
    height: f32,
    scale: f32,
    mode: ProofMode,
) -> Result<u64, String> {
    submit_packet(
        pane,
        &proof_packet(width, height, scale, mode, FrameIdentity::Initial),
    )
}

fn record_untagged_result(result: &Result<u64, String>) {
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
        assert_eq!(ProofMode::for_instance(SLOW_ACTOR_ID), ProofMode::Slow);
        assert_eq!(
            ProofMode::for_instance(STALL_ACTOR_ID),
            ProofMode::StallOnce
        );
        assert_eq!(
            ProofMode::for_instance(STALL_AFTER_FIRST_ACTOR_ID),
            ProofMode::StallAfterFirst
        );
        assert_eq!(ProofMode::for_instance("resize-proof"), ProofMode::Fast);
    }

    #[test]
    fn pending_epoch_replacement_never_draws_the_obsolete_target() {
        let mut scheduler = DelayScheduler::new();
        let first = target(11, 320.0);
        let final_target = target(12, 480.0);
        let first_worker = match scheduler.offer(first) {
            DelayAction::Spawn { worker, target } if target == first => Some(worker),
            _ => None,
        };
        assert!(first_worker.is_some());
        assert_eq!(scheduler.offer(final_target), DelayAction::None);
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
    fn frame_fiducials_change_for_epoch_and_keep_exact_extent_markers() {
        let initial = proof_packet(320.0, 240.0, 1.0, ProofMode::Fast, FrameIdentity::Initial);
        let tagged = proof_packet(480.0, 300.0, 1.25, ProofMode::Fast, FrameIdentity::Epoch(7));
        assert_ne!(initial.commands, tagged.commands);
        assert_eq!(initial.commands.len(), tagged.commands.len());
        assert!(initial.commands.len() > 160);
    }
}
