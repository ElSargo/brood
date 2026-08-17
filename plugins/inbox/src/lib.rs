use std::{collections::VecDeque, sync::Mutex};

use brood_graphics::{Color, Command as DrawCommand, Packet, Rect, theme::ashen};
use brood_plugin_sdk::{
    bindings,
    exports::brood::plugin::actor::{
        Command, Event, Guest, LifecycleEvent, LocalActorMessage, PresentationEvent,
    },
    graphics::{
        UiFrame, reuse_pane_content,
    },
    input::BUTTON_PRIMARY as LEFT_BUTTON,
    text::one_line,
};
use serde::Deserialize;

const INIT_ACTOR_ID: &str = "brood.client-init";
const SHELL_ACTOR_ID: &str = "session-switcher";
const INBOX_ACTOR_ID: &str = "inbox";
const HEADER_HEIGHT: f32 = 48.0;
const ITEM_HEIGHT: f32 = 72.0;
const MAX_ITEMS: usize = 32;
const MAX_TITLE_CHARS: usize = 96;
const MAX_DETAIL_CHARS: usize = 160;
const SHOW_MESSAGE: &[u8] = br#"{"kind":"inbox-action","action":"show"}"#;

const BG: Color = ashen::BACKGROUND;
const SURFACE: Color = ashen::SURFACE;
const SURFACE_RAISED: Color = ashen::SURFACE_RAISED;
const HOVER: Color = ashen::INTERACTIVE_HOVER;
const PRESSED: Color = ashen::INTERACTIVE_PRESSED;
const BORDER: Color = ashen::BORDER;
const TEXT: Color = ashen::TEXT_STRONG;
const FG: Color = ashen::TEXT;
const MUTED: Color = ashen::MUTED;
const ACCENT: Color = ashen::ACCENT;
const INFO: Color = ashen::INFO;
const ERROR: Color = ashen::ERROR;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct InboxMessage {
    kind: String,
    category: String,
    title: String,
    detail: String,
    target_actor: String,
    status: String,
}

#[derive(Clone)]
struct InboxItem {
    category: String,
    title: String,
    detail: String,
    target_actor: String,
    failed: bool,
}

struct InboxState {
    pane: u64,
    width: f32,
    height: f32,
    resize_extent: (f32, f32, f32),
    items: VecDeque<InboxItem>,
    scroll: usize,
    text_resources: Vec<u64>,
    error: String,
    hovered: Option<usize>,
    pressed: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResizePaintPlan {
    OrdinaryRedraw,
    TaggedRedraw(u64),
    Reuse(u64),
}

impl InboxState {
    const fn new() -> Self {
        Self {
            pane: 0,
            width: 320.0,
            height: 720.0,
            resize_extent: (320.0, 720.0, 1.0),
            items: VecDeque::new(),
            scroll: 0,
            text_resources: Vec::new(),
            error: String::new(),
            hovered: None,
            pressed: None,
        }
    }
}

static STATE: Mutex<InboxState> = Mutex::new(InboxState::new());

struct InboxPlugin;

impl Guest for InboxPlugin {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(context)) => {
                if context.instance_id != INBOX_ACTOR_ID
                    || context.backend_id.is_some()
                    || context.session_id.is_some()
                {
                    return vec![Command::LifecycleComplete(Err(
                        "inbox must be started as presentation-global state".to_string(),
                    ))];
                }
                vec![Command::LifecycleComplete(Ok(()))]
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![Command::LifecycleComplete(Ok(()))]
            }
            Event::Presentation(event) => handle_presentation(event),
            Event::Mailbox(message) => handle_mailbox(&message.sender, &message.body),
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

fn handle_mailbox(sender: &str, body: &[u8]) -> Vec<Command> {
    if sender == "key-policy" && body == SHOW_MESSAGE {
        let Ok(state) = STATE.lock() else {
            return Vec::new();
        };
        return show_pane_commands(state.pane);
    }
    let Some(item) = parse_item(sender, body) else {
        return Vec::new();
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    push_item(&mut state.items, item);
    state.scroll = 0;
    state.hovered = None;
    state.pressed = None;
    redraw_or_record_error(&mut state);
    Vec::new()
}

fn show_pane_commands(pane: u64) -> Vec<Command> {
    if pane == 0 {
        return Vec::new();
    }
    vec![Command::SendLocalMessage(LocalActorMessage {
        actor: SHELL_ACTOR_ID.to_string(),
        body: format!(r#"{{"kind":"inbox-show","pane":{pane}}}"#).into_bytes(),
    })]
}

fn parse_item(sender: &str, body: &[u8]) -> Option<InboxItem> {
    if !sender.starts_with("session:") {
        return None;
    }
    let message = serde_json::from_slice::<InboxMessage>(body).ok()?;
    if message.kind != "inbox-push"
        || !valid_actor(&message.target_actor)
        || message.title.trim().is_empty()
    {
        return None;
    }
    Some(InboxItem {
        category: one_line(&message.category, 24),
        title: one_line(&message.title, MAX_TITLE_CHARS),
        detail: one_line(&message.detail, MAX_DETAIL_CHARS),
        target_actor: message.target_actor,
        failed: message.status == "failed",
    })
}

fn push_item(items: &mut VecDeque<InboxItem>, item: InboxItem) {
    items.push_front(item);
    items.truncate(MAX_ITEMS);
}

fn handle_presentation(event: PresentationEvent) -> Vec<Command> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match event {
        PresentationEvent::PaneCreated(created) => {
            state.pane = created.pane;
            state.width = created.width.max(220.0);
            state.height = created.height.max(160.0);
            state.resize_extent = (created.width, created.height, created.scale);
            redraw_or_record_error(&mut state);
            vec![Command::SendLocalMessage(LocalActorMessage {
                actor: SHELL_ACTOR_ID.to_string(),
                body: format!(r#"{{"kind":"inbox-ready","pane":{}}}"#, state.pane).into_bytes(),
            })]
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            let width = resized.width.max(220.0);
            let height = resized.height.max(160.0);
            let target_extent = (resized.width, resized.height, resized.scale);
            let paint = resize_paint_plan(state.resize_extent, target_extent, resized.resize_epoch);
            state.width = width;
            state.height = height;
            state.resize_extent = target_extent;
            state.hovered = None;
            state.pressed = None;
            clamp_scroll(&mut state);
            paint_for_resize_or_record_error(&mut state, paint);
            Vec::new()
        }
        PresentationEvent::PointerMoved(moved) if moved.pane == state.pane => {
            let hovered = inbox_target_at(&state, moved.y);
            if state.hovered != hovered {
                state.hovered = hovered;
                redraw_or_record_error(&mut state);
            }
            Vec::new()
        }
        PresentationEvent::PointerLeft(left) if left.pane == state.pane => {
            if state.hovered.take().is_some() || state.pressed.take().is_some() {
                redraw_or_record_error(&mut state);
            }
            Vec::new()
        }
        PresentationEvent::PointerButton(button)
            if button.pane == state.pane && button.button == LEFT_BUTTON =>
        {
            let target = inbox_target_at(&state, button.y);
            if button.pressed {
                state.hovered = target;
                state.pressed = target;
                redraw_or_record_error(&mut state);
                return Vec::new();
            }
            let pressed = state.pressed.take();
            state.hovered = target;
            let commands = match (pressed, target) {
                (Some(pressed), Some(released)) if pressed == released => {
                    let Some(item) = state.items.remove(pressed) else {
                        return Vec::new();
                    };
                    clamp_scroll(&mut state);
                    vec![Command::SendLocalMessage(LocalActorMessage {
                        actor: item.target_actor,
                        body: br#"{"kind":"focus"}"#.to_vec(),
                    })]
                }
                _ => Vec::new(),
            };
            redraw_or_record_error(&mut state);
            commands
        }
        PresentationEvent::PointerScroll(scroll) if scroll.pane == state.pane => {
            let maximum = state.items.len().saturating_sub(visible_item_count(&state));
            if scroll.vertical < 0.0 {
                state.scroll = (state.scroll + 1).min(maximum);
            } else if scroll.vertical > 0.0 {
                state.scroll = state.scroll.saturating_sub(1);
            }
            state.hovered = None;
            state.pressed = None;
            redraw_or_record_error(&mut state);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn visible_item_count(state: &InboxState) -> usize {
    ((state.height - HEADER_HEIGHT) / ITEM_HEIGHT)
        .floor()
        .max(1.0) as usize
}

fn inbox_target_at(state: &InboxState, y: f32) -> Option<usize> {
    if !y.is_finite() || y < HEADER_HEIGHT || y >= state.height {
        return None;
    }
    let row = ((y - HEADER_HEIGHT) / ITEM_HEIGHT).floor() as usize;
    (row < visible_item_count(state))
        .then_some(state.scroll + row)
        .filter(|index| *index < state.items.len())
}

fn clamp_scroll(state: &mut InboxState) {
    state.scroll = state
        .scroll
        .min(state.items.len().saturating_sub(visible_item_count(state)));
}

fn redraw_or_record_error(state: &mut InboxState) {
    redraw_for_resize_or_record_error(state, None);
}

fn resize_paint_plan(
    current: (f32, f32, f32),
    target: (f32, f32, f32),
    resize_epoch: Option<u64>,
) -> ResizePaintPlan {
    match resize_epoch {
        Some(resize_epoch) if current == target => ResizePaintPlan::Reuse(resize_epoch),
        Some(resize_epoch) => ResizePaintPlan::TaggedRedraw(resize_epoch),
        None => ResizePaintPlan::OrdinaryRedraw,
    }
}

fn paint_for_resize_or_record_error(state: &mut InboxState, paint: ResizePaintPlan) {
    let result = match paint {
        ResizePaintPlan::OrdinaryRedraw => redraw(state, None),
        ResizePaintPlan::TaggedRedraw(resize_epoch) => redraw(state, Some(resize_epoch)),
        ResizePaintPlan::Reuse(resize_epoch) => reuse_pane_content(state.pane, resize_epoch)
            .or_else(|_reuse_error| redraw(state, Some(resize_epoch)).map(|_| ())),
    };
    if let Err(error) = result {
        state.error = error;
    }
}

fn redraw_for_resize_or_record_error(state: &mut InboxState, resize_epoch: Option<u64>) {
    if let Err(error) = redraw(state, resize_epoch) {
        state.error = error;
    }
}

fn redraw(state: &mut InboxState, resize_epoch: Option<u64>) -> Result<(), String> {
    if state.pane == 0 {
        return Ok(());
    }
    let mut packet = Packet::new();
    let mut frame = UiFrame::new(state.pane);
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, state.height),
        color: BG,
    });
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, HEADER_HEIGHT),
        color: SURFACE,
    });
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, HEADER_HEIGHT - 1.0, state.width, 1.0),
        color: BORDER,
    });
    add_text(
        state.pane,
        &mut packet,
        &mut frame,
        "Inbox",
        17.0,
        TEXT,
        14.0,
        12.0,
        state.width - 80.0,
    )?;
    add_text(
        state.pane,
        &mut packet,
        &mut frame,
        &state.items.len().to_string(),
        12.0,
        ACCENT,
        state.width - 42.0,
        15.0,
        28.0,
    )?;

    if state.items.is_empty() {
        add_text(
            state.pane,
            &mut packet,
            &mut frame,
            "No notifications",
            14.0,
            MUTED,
            14.0,
            68.0,
            state.width - 28.0,
        )?;
    } else {
        let visible = visible_item_count(state);
        for (index, item) in state
            .items
            .iter()
            .skip(state.scroll)
            .take(visible)
            .enumerate()
        {
            let y = HEADER_HEIGHT + index as f32 * ITEM_HEIGHT;
            let absolute_index = state.scroll + index;
            let pressed = state.pressed == Some(absolute_index);
            let hovered = state.hovered == Some(absolute_index);
            packet.push(DrawCommand::FillRoundedRect {
                rect: Rect::new(8.0, y + 7.0, state.width - 16.0, ITEM_HEIGHT - 12.0),
                radius: 5.0,
                color: if pressed {
                    PRESSED
                } else if hovered {
                    HOVER
                } else {
                    SURFACE_RAISED
                },
            });
            packet.push(DrawCommand::FillRect {
                rect: Rect::new(8.0, y + 7.0, 3.0, ITEM_HEIGHT - 12.0),
                color: if item.failed { ERROR } else { INFO },
            });
            let category = if item.category.is_empty() {
                "update"
            } else {
                &item.category
            };
            add_text(
                state.pane,
                &mut packet,
                &mut frame,
                category,
                10.0,
                if item.failed { ERROR } else { ACCENT },
                18.0,
                y + 11.0,
                state.width - 36.0,
            )?;
            if hovered || pressed {
                add_text(
                    state.pane,
                    &mut packet,
                    &mut frame,
                    if pressed { "Opening…" } else { "Open  ›" },
                    10.0,
                    ACCENT,
                    (state.width - 74.0).max(18.0),
                    y + 11.0,
                    58.0,
                )?;
            }
            add_text(
                state.pane,
                &mut packet,
                &mut frame,
                &truncate(&item.title, state.width, 13.0),
                13.0,
                FG,
                18.0,
                y + 27.0,
                state.width - 36.0,
            )?;
            add_text(
                state.pane,
                &mut packet,
                &mut frame,
                &truncate(&item.detail, state.width, 11.0),
                11.0,
                MUTED,
                18.0,
                y + 45.0,
                state.width - 36.0,
            )?;
        }
    }
    if !state.error.is_empty() {
        add_text(
            state.pane,
            &mut packet,
            &mut frame,
            &one_line(&state.error, 80),
            10.0,
            ERROR,
            12.0,
            state.height - 20.0,
            state.width - 24.0,
        )?;
    }
    frame.submit(resize_epoch, &packet, &mut state.text_resources)?;
    state.error.clear();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_text(
    _pane: u64,
    packet: &mut Packet,
    frame: &mut UiFrame,
    text: &str,
    size: f32,
    color: Color,
    x: f32,
    y: f32,
    width: f32,
) -> Result<(), String> {
    frame
        .draw_text_clipped(packet, text, size, width, size * 1.3, color, x, y)
        .map_err(|error| format!("layout inbox text: {error}"))?;
    Ok(())
}

fn valid_actor(actor: &str) -> bool {
    !actor.is_empty()
        && actor.len() <= 128
        && actor != INIT_ACTOR_ID
        && actor != INBOX_ACTOR_ID
        && actor
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn truncate(text: &str, width: f32, size: f32) -> String {
    let maximum = ((width - 36.0) / (size * 0.56)).max(8.0) as usize;
    let mut output = text.chars().take(maximum).collect::<String>();
    if text.chars().count() > maximum {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(title: &str, target: &str) -> Vec<u8> {
        format!(
            r#"{{"kind":"inbox-push","category":"agent","title":"{title}","detail":"done","target_actor":"{target}","status":"completed"}}"#
        )
        .into_bytes()
    }

    #[test]
    fn only_authenticated_session_messages_with_valid_targets_are_accepted() {
        let item = parse_item(
            "session:coding-agent-2",
            &message("Agent finished", "coding-agent-ui-b1-s1-2"),
        )
        .expect("authenticated completion");
        assert_eq!(item.title, "Agent finished");
        assert_eq!(item.target_actor, "coding-agent-ui-b1-s1-2");
        assert!(!item.failed);

        assert!(parse_item("coding-agent-2", &message("done", "agent-ui")).is_none());
        assert!(parse_item("session:agent", b"not json").is_none());
        assert!(parse_item("session:agent", &message("", "agent-ui")).is_none());
        assert!(parse_item("session:agent", &message("done", "../agent-ui")).is_none());
    }

    #[test]
    fn queue_keeps_the_newest_bounded_items() {
        let mut items = VecDeque::new();
        for index in 0..(MAX_ITEMS + 3) {
            push_item(
                &mut items,
                InboxItem {
                    category: "test".to_string(),
                    title: index.to_string(),
                    detail: String::new(),
                    target_actor: "target".to_string(),
                    failed: false,
                },
            );
        }
        assert_eq!(items.len(), MAX_ITEMS);
        assert_eq!(items.front().map(|item| item.title.as_str()), Some("34"));
        assert_eq!(items.back().map(|item| item.title.as_str()), Some("3"));
    }

    #[test]
    fn show_action_asks_the_root_shell_to_reveal_the_global_inbox_pane() {
        assert!(show_pane_commands(0).is_empty());
        let commands = show_pane_commands(42);
        assert!(matches!(
            commands.as_slice(),
            [Command::SendLocalMessage(LocalActorMessage { actor, body })]
                if actor == SHELL_ACTOR_ID
                    && body == br#"{"kind":"inbox-show","pane":42}"#
        ));
    }

    #[test]
    fn synchronized_resize_reuses_equal_extents_and_tags_changed_extents() {
        assert_eq!(
            resize_paint_plan((320.0, 720.0, 1.0), (320.0, 720.0, 1.0), Some(41)),
            ResizePaintPlan::Reuse(41)
        );
        assert_eq!(
            resize_paint_plan((320.0, 720.0, 1.0), (300.0, 720.0, 1.0), Some(42)),
            ResizePaintPlan::TaggedRedraw(42)
        );
        assert_eq!(
            resize_paint_plan((320.0, 720.0, 1.0), (300.0, 720.0, 1.0), None),
            ResizePaintPlan::OrdinaryRedraw
        );
        assert_eq!(
            resize_paint_plan((200.0, 160.0, 1.0), (210.0, 160.0, 1.0), Some(43)),
            ResizePaintPlan::TaggedRedraw(43),
            "clamped UI dimensions must not hide a real allocation change"
        );
        assert_eq!(
            resize_paint_plan((320.0, 720.0, 1.0), (320.0, 720.0, 2.0), Some(44)),
            ResizePaintPlan::TaggedRedraw(44),
            "content from a different output scale is not reusable"
        );
    }
}

bindings::export!(InboxPlugin with_types_in bindings);
