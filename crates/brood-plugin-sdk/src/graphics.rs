//! Canvas submission and text-resource helpers.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use brood_graphics::{Color, Packet};

use crate::brood::plugin::host;
use crate::exports::brood::plugin::actor;

pub use brood_graphics::{Command, PacketError, Rect, theme};

static DEFERRED_RESOURCE_DROPS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
static ACTIVE_ACTOR_FRAME: Mutex<Option<ActorFrame>> = Mutex::new(None);
static PENDING_RESIZE_EPOCHS: Mutex<BTreeMap<u64, u64>> = Mutex::new(BTreeMap::new());
const MAX_ACTOR_PANES: usize = 16;

/// One owned draw retained until an actor-frame batch is finished.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActorFrameDraw {
    pub pane: u64,
    pub resize_epoch: Option<u64>,
    pub state_watermark: Option<u64>,
    pub packet: Vec<u8>,
}

/// Builder for one atomic, actor-scoped frame submission.
///
/// Use [`Self::from_render_hint`] for paced rendering. Every submission needs
/// a host-issued opportunity; there is deliberately no unhinted constructor.
#[derive(Debug)]
pub struct ActorFrame {
    token: host::RenderToken,
    draws: Vec<ActorFrameDraw>,
    panes: BTreeSet<u64>,
    consumed_resize_epochs: BTreeMap<u64, u64>,
    finished: bool,
}

impl ActorFrame {
    #[must_use]
    pub const fn from_render_hint(hint: &actor::RenderHint) -> Self {
        Self {
            token: host::RenderToken {
                opaque: hint.token.opaque,
            },
            draws: Vec::new(),
            panes: BTreeSet::new(),
            consumed_resize_epochs: BTreeMap::new(),
            finished: false,
        }
    }

    /// Add one changed pane. Duplicate panes are rejected before the host call.
    pub fn draw(
        &mut self,
        pane: u64,
        resize_epoch: Option<u64>,
        packet: &Packet,
    ) -> Result<(), String> {
        let packet = packet
            .encode()
            .map_err(|error| format!("encode plugin draw packet: {error}"))?;
        self.draw_encoded(pane, resize_epoch, packet)
    }

    /// Add one already encoded changed pane.
    pub fn draw_encoded(
        &mut self,
        pane: u64,
        resize_epoch: Option<u64>,
        packet: Vec<u8>,
    ) -> Result<(), String> {
        self.draw_encoded_with_state_watermark(pane, resize_epoch, None, packet)
    }

    /// Add one encoded pane with the exact causal state used to build it.
    pub fn draw_encoded_with_state_watermark(
        &mut self,
        pane: u64,
        resize_epoch: Option<u64>,
        state_watermark: Option<u64>,
        packet: Vec<u8>,
    ) -> Result<(), String> {
        if self.finished {
            return Err("actor frame is already finished".to_owned());
        }
        if !self.panes.contains(&pane) && self.panes.len() >= MAX_ACTOR_PANES {
            return Err(format!(
                "actor frame contains more than {MAX_ACTOR_PANES} panes"
            ));
        }
        if !self.panes.insert(pane) {
            return Err(format!("actor frame contains duplicate pane {pane}"));
        }
        self.draws.push(ActorFrameDraw {
            pane,
            resize_epoch,
            state_watermark,
            packet,
        });
        Ok(())
    }

    /// Finish the complete actor frame, including an empty no-change frame.
    pub fn finish(&mut self) -> Result<u64, String> {
        self.finish_raw()
            .map_err(|error| format!("finish actor frame: {error:?}"))
    }

    fn finish_raw(&mut self) -> Result<u64, host::GraphicsError> {
        if self.finished {
            return Err(host::GraphicsError::InvalidToken);
        }
        let revision = finish_encoded_actor_frame_raw(self.token, &self.draws)?;
        self.finished = true;
        Ok(revision)
    }
}

impl Drop for ActorFrame {
    fn drop(&mut self) {
        if self.finished || self.consumed_resize_epochs.is_empty() {
            return;
        }
        let mut pending = PENDING_RESIZE_EPOCHS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        restore_consumed_resize_epochs(&mut pending, &self.consumed_resize_epochs);
    }
}

fn restore_consumed_resize_epochs(pending: &mut BTreeMap<u64, u64>, consumed: &BTreeMap<u64, u64>) {
    for (pane, resize_epoch) in consumed {
        pending.entry(*pane).or_insert(*resize_epoch);
    }
}

/// Whether the current dispatch is reducing a render hint.
#[must_use]
pub fn render_is_active() -> bool {
    ACTIVE_ACTOR_FRAME
        .lock()
        .is_ok_and(|active| active.is_some())
}

/// Run visual construction for one hint and finish its complete actor batch.
/// Pane queue helpers are valid only inside this scope.
pub fn render_with_hint<T>(
    hint: &actor::RenderHint,
    render: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    {
        let mut active = ACTIVE_ACTOR_FRAME
            .lock()
            .map_err(|_poisoned| "actor frame scope is poisoned".to_owned())?;
        if active.is_some() {
            return Err("actor frame scope is already active".to_owned());
        }
        *active = Some(ActorFrame::from_render_hint(hint));
    }

    let rendered = render();
    let mut frame = ACTIVE_ACTOR_FRAME
        .lock()
        .map_err(|_poisoned| "actor frame scope is poisoned".to_owned())?
        .take()
        .ok_or_else(|| "actor frame scope disappeared".to_owned())?;
    let value = rendered?;
    match frame.finish_raw() {
        Ok(_) => {}
        Err(host::GraphicsError::StaleResizeEpoch) => {
            for draw in &mut frame.draws {
                draw.resize_epoch = None;
            }
            frame.finish()?;
        }
        Err(error) => return Err(format!("finish actor frame: {error:?}")),
    }
    Ok(value)
}

/// Request one coalesced render opportunity for the current actor.
pub fn request_render() -> Result<(), String> {
    host::render_request().map_err(|error| format!("request actor render: {error:?}"))
}

/// Remember synchronized-resize metadata for the actor's next hinted batch.
pub fn request_render_for_resize(pane: u64, resize_epoch: Option<u64>) -> Result<(), String> {
    if let Some(resize_epoch) = resize_epoch {
        let mut pending = PENDING_RESIZE_EPOCHS
            .lock()
            .map_err(|_poisoned| "pending resize epochs are poisoned".to_owned())?;
        remember_resize_epoch(&mut pending, pane, resize_epoch)?;
    }
    request_render()
}

fn remember_resize_epoch(
    pending: &mut BTreeMap<u64, u64>,
    pane: u64,
    resize_epoch: u64,
) -> Result<(), String> {
    if !pending.contains_key(&pane) && pending.len() >= MAX_ACTOR_PANES {
        return Err(format!(
            "actor has more than {MAX_ACTOR_PANES} pending pane resize epochs"
        ));
    }
    pending.insert(pane, resize_epoch);
    Ok(())
}

/// Enable or disable continuous render opportunities for the current actor.
pub fn set_render_subscription(enabled: bool) -> Result<(), String> {
    host::render_set_subscription(enabled)
        .map_err(|error| format!("set actor render subscription: {error:?}"))
}

/// Construct the common UI text shape while keeping width bounded.
#[must_use]
pub const fn ui_text_options(
    font_size: f32,
    max_width: f32,
    line_height: f32,
    color: Color,
) -> host::TextLayoutOptions {
    host::TextLayoutOptions {
        font_intent: host::FontIntent::Ui,
        font_weight: host::FontWeight::Normal,
        font_style: host::FontStyle::Normal,
        font_size,
        max_width: if max_width > 1.0 { max_width } else { 1.0 },
        line_height,
        red: color.r,
        green: color.g,
        blue: color.b,
        alpha: color.a,
    }
}

/// Shape one bounded UI label.
pub fn layout_ui_text(
    pane: u64,
    text: &str,
    font_size: f32,
    max_width: f32,
    line_height: f32,
    color: Color,
) -> Result<host::TextLayoutResult, String> {
    host::graphics_layout_text(
        pane,
        text,
        ui_text_options(font_size, max_width, line_height, color),
    )
    .map_err(|error| format!("layout plugin text: {error:?}"))
}

/// Resources created while constructing one replacement frame.
///
/// Submission is transactional: newly-created resources are retired if the
/// packet is rejected, and resources from the previous frame are retired only
/// after the replacement packet has been accepted.
pub struct UiFrame {
    pane: u64,
    resources: Vec<u64>,
}

impl UiFrame {
    #[must_use]
    pub const fn new(pane: u64) -> Self {
        Self {
            pane,
            resources: Vec::new(),
        }
    }

    pub fn layout_text(
        &mut self,
        text: &str,
        font_size: f32,
        max_width: f32,
        line_height: f32,
        color: Color,
    ) -> Result<host::TextLayoutResult, String> {
        let layout = layout_ui_text(self.pane, text, font_size, max_width, line_height, color)?;
        self.resources.push(layout.handle);
        Ok(layout)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "drawing text requires its full layout and placement specification"
    )]
    pub fn draw_text_clipped(
        &mut self,
        packet: &mut Packet,
        text: &str,
        font_size: f32,
        max_width: f32,
        line_height: f32,
        color: Color,
        x: f32,
        y: f32,
    ) -> Result<host::TextLayoutResult, String> {
        let layout = self.layout_text(text, font_size, max_width, line_height, color)?;
        draw_layout_clipped(packet, layout.handle, x, y, max_width, line_height);
        Ok(layout)
    }

    pub fn queue(
        mut self,
        resize_epoch: Option<u64>,
        packet: &Packet,
        active_resources: &mut Vec<u64>,
    ) -> Result<(), String> {
        match queue_packet_for_resize(self.pane, resize_epoch, packet) {
            Ok(()) => {
                drop_resources(self.pane, active_resources.drain(..));
                *active_resources = std::mem::take(&mut self.resources);
                Ok(())
            }
            Err(error) => {
                drop_resources(self.pane, self.resources.drain(..));
                Err(error)
            }
        }
    }
}

impl Drop for UiFrame {
    fn drop(&mut self) {
        drop_resources(self.pane, self.resources.drain(..));
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TextLayoutKey {
    text: String,
    font_size: u32,
    max_width: u32,
    line_height: u32,
    color: [u32; 4],
}

impl TextLayoutKey {
    fn new(text: &str, font_size: f32, max_width: f32, line_height: f32, color: Color) -> Self {
        Self {
            text: text.to_owned(),
            font_size: font_size.to_bits(),
            max_width: max_width.max(1.0).to_bits(),
            line_height: line_height.to_bits(),
            color: [
                color.r.to_bits(),
                color.g.to_bits(),
                color.b.to_bits(),
                color.a.to_bits(),
            ],
        }
    }
}

/// Cross-frame text layout cache with transactional resource retirement.
///
/// Call [`Self::begin_frame`] before drawing, use [`Self::layout_text`] for
/// every text run, and finish with [`Self::submit`]. Entries unused by an
/// accepted frame are retired. Entries created for a rejected frame are
/// rolled back while the last accepted frame remains intact.
pub struct TextLayoutCache {
    pane: u64,
    layouts: BTreeMap<TextLayoutKey, host::TextLayoutResult>,
    used: BTreeSet<TextLayoutKey>,
    created: BTreeSet<TextLayoutKey>,
}

impl TextLayoutCache {
    #[must_use]
    pub const fn new(pane: u64) -> Self {
        Self {
            pane,
            layouts: BTreeMap::new(),
            used: BTreeSet::new(),
            created: BTreeSet::new(),
        }
    }

    pub fn begin_frame(&mut self) {
        self.abort_frame();
        self.used.clear();
    }

    /// Roll back resources first introduced by the in-progress frame.
    pub fn abort_frame(&mut self) {
        let created = std::mem::take(&mut self.created);
        self.retire(created);
        self.used.clear();
    }

    pub fn set_pane(&mut self, pane: u64) {
        if self.pane == pane {
            return;
        }
        let resources = std::mem::take(&mut self.layouts)
            .into_values()
            .map(|layout| layout.handle);
        drop_resources(self.pane, resources);
        self.used.clear();
        self.created.clear();
        self.pane = pane;
    }

    pub fn layout_text(
        &mut self,
        text: &str,
        font_size: f32,
        max_width: f32,
        line_height: f32,
        color: Color,
    ) -> Result<host::TextLayoutResult, String> {
        let key = TextLayoutKey::new(text, font_size, max_width, line_height, color);
        self.used.insert(key.clone());
        if let Some(layout) = self.layouts.get(&key) {
            return Ok(*layout);
        }
        let layout = layout_ui_text(self.pane, text, font_size, max_width, line_height, color)?;
        self.layouts.insert(key.clone(), layout);
        self.created.insert(key);
        Ok(layout)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "drawing text requires its full layout and placement specification"
    )]
    pub fn draw_text_clipped(
        &mut self,
        packet: &mut Packet,
        text: &str,
        font_size: f32,
        max_width: f32,
        line_height: f32,
        color: Color,
        x: f32,
        y: f32,
    ) -> Result<host::TextLayoutResult, String> {
        let layout = self.layout_text(text, font_size, max_width, line_height, color)?;
        draw_layout_clipped(packet, layout.handle, x, y, max_width, line_height);
        Ok(layout)
    }

    pub fn queue(&mut self, resize_epoch: Option<u64>, packet: &Packet) -> Result<(), String> {
        match queue_packet_for_resize(self.pane, resize_epoch, packet) {
            Ok(()) => {
                let stale = self
                    .layouts
                    .keys()
                    .filter(|key| !self.used.contains(*key))
                    .cloned()
                    .collect::<Vec<_>>();
                self.retire(stale);
                self.created.clear();
                self.used.clear();
                Ok(())
            }
            Err(error) => {
                self.abort_frame();
                Err(error)
            }
        }
    }

    fn retire(&mut self, keys: impl IntoIterator<Item = TextLayoutKey>) {
        let resources = keys
            .into_iter()
            .filter_map(|key| self.layouts.remove(&key).map(|layout| layout.handle));
        drop_resources(self.pane, resources);
    }
}

fn draw_layout_clipped(
    packet: &mut Packet,
    layout: u64,
    x: f32,
    y: f32,
    max_width: f32,
    line_height: f32,
) {
    packet.push(Command::Save);
    packet.push(Command::ClipRect(Rect::new(
        x,
        y,
        max_width.max(1.0),
        line_height.max(1.0),
    )));
    packet.push(Command::DrawTextLayout { layout, x, y });
    packet.push(Command::Restore);
}

/// Queue one pane into the active hinted actor-frame batch.
pub fn queue_packet(pane: u64, packet: &Packet) -> Result<(), String> {
    queue_packet_for_resize(pane, None, packet)
}

/// Queue one synchronized-resize pane into the active actor-frame batch.
pub fn queue_tagged_packet(pane: u64, resize_epoch: u64, packet: &Packet) -> Result<(), String> {
    queue_packet_for_resize(pane, Some(resize_epoch), packet)
}

/// Queue one pane with optional synchronized-resize metadata.
pub fn queue_packet_for_resize(
    pane: u64,
    resize_epoch: Option<u64>,
    packet: &Packet,
) -> Result<(), String> {
    let bytes = packet
        .encode()
        .map_err(|error| format!("encode plugin draw packet: {error}"))?;
    queue_encoded_packet_for_resize(pane, resize_epoch, bytes)
}

/// Queue an already encoded pane into the active hinted actor-frame batch.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the actor-frame guard must remain held while resize metadata is consumed so a concurrent frame close cannot lose that metadata"
)]
pub fn queue_encoded_packet_for_resize(
    pane: u64,
    resize_epoch: Option<u64>,
    bytes: Vec<u8>,
) -> Result<(), String> {
    queue_encoded_packet_for_resize_with_state_watermark(pane, resize_epoch, None, bytes)
}

/// Queue an encoded pane with the exact causal state used to build it.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the actor-frame guard must remain held while resize metadata is consumed so a concurrent frame close cannot lose that metadata"
)]
pub fn queue_encoded_packet_for_resize_with_state_watermark(
    pane: u64,
    resize_epoch: Option<u64>,
    state_watermark: Option<u64>,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let mut active = ACTIVE_ACTOR_FRAME
        .lock()
        .map_err(|_poisoned| "actor frame scope is poisoned".to_owned())?;
    let frame = active
        .as_mut()
        .ok_or_else(|| "pane draw requires an active render hint".to_owned())?;
    let remembered = if resize_epoch.is_none() {
        PENDING_RESIZE_EPOCHS
            .lock()
            .map_err(|_poisoned| "pending resize epochs are poisoned".to_owned())?
            .remove(&pane)
    } else {
        None
    };
    let resize_epoch = resize_epoch.or(remembered);
    match frame.draw_encoded_with_state_watermark(pane, resize_epoch, state_watermark, bytes) {
        Ok(()) => {
            if let Some(resize_epoch) = remembered {
                frame.consumed_resize_epochs.insert(pane, resize_epoch);
            }
            Ok(())
        }
        Err(error) => {
            if let Some(resize_epoch) = remembered {
                let mut pending = PENDING_RESIZE_EPOCHS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.entry(pane).or_insert(resize_epoch);
            }
            Err(error)
        }
    }
}

fn finish_encoded_actor_frame_raw(
    token: host::RenderToken,
    draws: &[ActorFrameDraw],
) -> Result<u64, host::GraphicsError> {
    let draws = draws
        .iter()
        .map(|draw| host::ActorFrameDraw {
            pane: draw.pane,
            resize_epoch: draw.resize_epoch,
            state_watermark: draw.state_watermark,
            packet: draw.packet.clone(),
        })
        .collect::<Vec<_>>();
    host::graphics_finish_actor_frame(token, &draws)
}

/// Mark one synchronized resize ready using the pane's current visible content.
pub fn reuse_pane_content(pane: u64, resize_epoch: u64) -> Result<(), String> {
    host::graphics_reuse_pane_content(pane, resize_epoch)
        .map_err(|error| format!("reuse plugin pane content: {error:?}"))
}

/// Best-effort release of resources no longer referenced by the active packet.
///
/// Resources still held by visible or staged content are retried on the next
/// call, after a synchronized resize has had an opportunity to promote.
pub fn drop_resources(pane: u64, resources: impl IntoIterator<Item = u64>) {
    let mut pending = DEFERRED_RESOURCE_DROPS.lock().map_or_else(
        |_poisoned| Vec::new(),
        |mut pending| std::mem::take(&mut *pending),
    );
    pending.extend(resources.into_iter().map(|resource| (pane, resource)));

    let mut deferred = Vec::new();
    for (pane, resource) in pending {
        if matches!(
            host::graphics_drop_resource(pane, resource),
            Err(host::GraphicsError::ResourceInUse)
        ) {
            deferred.push((pane, resource));
        }
    }
    if let Ok(mut pending) = DEFERRED_RESOURCE_DROPS.lock() {
        pending.extend(deferred);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_frame_rejects_duplicate_panes_before_submission() {
        let hint = actor::RenderHint {
            token: actor::RenderToken { opaque: 4 },
            target_presentation_time_ns: 10,
            submission_deadline_ns: 9,
            relevant_panes: vec![7],
            event_watermark: 3,
        };
        let mut frame = ActorFrame::from_render_hint(&hint);
        assert!(frame.draw_encoded(7, None, vec![1]).is_ok());
        assert_eq!(
            frame.draw_encoded(7, Some(4), vec![2]),
            Err("actor frame contains duplicate pane 7".to_owned())
        );
    }

    #[test]
    fn actor_frame_rejects_more_than_the_bounded_actor_pane_set() {
        let hint = actor::RenderHint {
            token: actor::RenderToken { opaque: 5 },
            target_presentation_time_ns: 10,
            submission_deadline_ns: 9,
            relevant_panes: Vec::new(),
            event_watermark: 3,
        };
        let mut frame = ActorFrame::from_render_hint(&hint);
        for pane in 0..MAX_ACTOR_PANES as u64 {
            assert!(frame.draw_encoded(pane, None, vec![1]).is_ok());
        }
        assert_eq!(
            frame.draw_encoded(MAX_ACTOR_PANES as u64, None, vec![1]),
            Err(format!(
                "actor frame contains more than {MAX_ACTOR_PANES} panes"
            ))
        );
    }

    #[test]
    fn actor_frame_draws_default_to_no_watermark_and_preserve_explicit_state() {
        let hint = actor::RenderHint {
            token: actor::RenderToken { opaque: 6 },
            target_presentation_time_ns: 10,
            submission_deadline_ns: 9,
            relevant_panes: vec![7, 8],
            event_watermark: 3,
        };
        let mut frame = ActorFrame::from_render_hint(&hint);
        assert!(frame.draw_encoded(7, None, vec![1]).is_ok());
        assert!(
            frame
                .draw_encoded_with_state_watermark(8, None, Some(720), vec![2])
                .is_ok()
        );
        assert_eq!(
            frame.draws.first().map(|draw| draw.state_watermark),
            Some(None)
        );
        assert_eq!(
            frame.draws.get(1).map(|draw| draw.state_watermark),
            Some(Some(720))
        );
    }

    #[test]
    fn pending_resize_epochs_are_bounded_and_existing_panes_can_update() {
        let mut pending = BTreeMap::new();
        for pane in 0..MAX_ACTOR_PANES as u64 {
            assert!(remember_resize_epoch(&mut pending, pane, pane + 10).is_ok());
        }
        assert!(remember_resize_epoch(&mut pending, 99, 1).is_err());
        assert!(remember_resize_epoch(&mut pending, 0, 77).is_ok());
        assert_eq!(pending.get(&0), Some(&77));
        assert_eq!(pending.len(), MAX_ACTOR_PANES);
    }

    #[test]
    fn rejected_and_abandoned_frames_restore_pending_resize_metadata() {
        let mut active = ACTIVE_ACTOR_FRAME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active = None;
        let mut pending = PENDING_RESIZE_EPOCHS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.clear();
        pending.insert(7, 41);
        drop(pending);
        drop(active);

        assert_eq!(
            queue_encoded_packet_for_resize(7, None, vec![1]),
            Err("pane draw requires an active render hint".to_owned())
        );
        let pending = PENDING_RESIZE_EPOCHS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(pending.get(&7), Some(&41));
        drop(pending);

        let hint = actor::RenderHint {
            token: actor::RenderToken { opaque: 8 },
            target_presentation_time_ns: 10,
            submission_deadline_ns: 9,
            relevant_panes: vec![7],
            event_watermark: 3,
        };
        let mut active = ACTIVE_ACTOR_FRAME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active = Some(ActorFrame::from_render_hint(&hint));
        drop(active);
        assert!(queue_encoded_packet_for_resize(7, None, vec![1]).is_ok());
        let abandoned = ACTIVE_ACTOR_FRAME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(abandoned);
        let mut pending = PENDING_RESIZE_EPOCHS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(pending.get(&7), Some(&41));
        pending.clear();
    }

    #[test]
    fn abandoned_frame_restores_consumed_resize_without_overwriting_newer_metadata() {
        let mut pending = BTreeMap::from([(7, 42)]);
        let consumed = BTreeMap::from([(7, 41), (8, 51)]);
        restore_consumed_resize_epochs(&mut pending, &consumed);
        assert_eq!(pending, BTreeMap::from([(7, 42), (8, 51)]));
    }
}
