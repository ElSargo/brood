wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use std::{collections::VecDeque, sync::Mutex};

use brood::plugin::host;
use brood_graphics::{Color, Command as DrawCommand, Packet, Rect, theme::ashen};
use exports::brood::plugin::actor::{
    Command, Event, Guest, LifecycleEvent, LocalActorMessage, PresentationEvent,
};
use serde::Deserialize;

const INIT_ACTOR_ID: &str = "brood.client-init";
const INBOX_ACTOR_ID: &str = "inbox";
const LEFT_BUTTON: u32 = 0x110;
const HEADER_HEIGHT: f32 = 48.0;
const ITEM_HEIGHT: f32 = 72.0;
const MAX_ITEMS: usize = 32;
const MAX_TITLE_CHARS: usize = 96;
const MAX_DETAIL_CHARS: usize = 160;
const SHOW_MESSAGE: &[u8] = br#"{"kind":"inbox-action","action":"show"}"#;

const BG: Color = ashen::BACKGROUND;
const SURFACE: Color = ashen::SURFACE;
const SURFACE_RAISED: Color = ashen::SURFACE_RAISED;
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
    items: VecDeque<InboxItem>,
    scroll: usize,
    text_resources: Vec<u64>,
    error: String,
}

impl InboxState {
    const fn new() -> Self {
        Self {
            pane: 0,
            width: 320.0,
            height: 720.0,
            items: VecDeque::new(),
            scroll: 0,
            text_resources: Vec::new(),
            error: String::new(),
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
        return focus_pane_command(state.pane);
    }
    let Some(item) = parse_item(sender, body) else {
        return Vec::new();
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    push_item(&mut state.items, item);
    state.scroll = 0;
    redraw_or_record_error(&mut state);
    Vec::new()
}

fn focus_pane_command(pane: u64) -> Vec<Command> {
    (pane != 0)
        .then(|| {
            Command::SendHostMessage(
                format!(r#"{{"type":"focus-pane","pane":{pane}}}"#).into_bytes(),
            )
        })
        .into_iter()
        .collect()
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
            redraw_or_record_error(&mut state);
            vec![Command::SendLocalMessage(LocalActorMessage {
                actor: INIT_ACTOR_ID.to_string(),
                body: format!(r#"{{"kind":"inbox-ready","pane":{}}}"#, state.pane).into_bytes(),
            })]
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            state.width = resized.width.max(220.0);
            state.height = resized.height.max(160.0);
            clamp_scroll(&mut state);
            redraw_or_record_error(&mut state);
            Vec::new()
        }
        PresentationEvent::PointerButton(button)
            if button.pane == state.pane && button.pressed && button.button == LEFT_BUTTON =>
        {
            let index = ((button.y - HEADER_HEIGHT) / ITEM_HEIGHT).floor() as isize;
            if index < 0 {
                return Vec::new();
            }
            let absolute_index = state.scroll + index as usize;
            let Some(item) = state.items.remove(absolute_index) else {
                return Vec::new();
            };
            clamp_scroll(&mut state);
            redraw_or_record_error(&mut state);
            vec![Command::SendLocalMessage(LocalActorMessage {
                actor: item.target_actor,
                body: br#"{"kind":"focus"}"#.to_vec(),
            })]
        }
        PresentationEvent::PointerScroll(scroll) if scroll.pane == state.pane => {
            let maximum = state.items.len().saturating_sub(visible_item_count(&state));
            if scroll.vertical < 0.0 {
                state.scroll = (state.scroll + 1).min(maximum);
            } else if scroll.vertical > 0.0 {
                state.scroll = state.scroll.saturating_sub(1);
            }
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

fn clamp_scroll(state: &mut InboxState) {
    state.scroll = state
        .scroll
        .min(state.items.len().saturating_sub(visible_item_count(state)));
}

fn redraw_or_record_error(state: &mut InboxState) {
    if let Err(error) = redraw(state) {
        state.error = error;
    }
}

fn redraw(state: &mut InboxState) -> Result<(), String> {
    if state.pane == 0 {
        return Ok(());
    }
    for resource in state.text_resources.drain(..) {
        let _ = host::graphics_drop_resource(state.pane, resource);
    }
    let mut packet = Packet::new();
    let mut resources = Vec::new();
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
        &mut resources,
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
        &mut resources,
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
            &mut resources,
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
            packet.push(DrawCommand::FillRoundedRect {
                rect: Rect::new(8.0, y + 7.0, state.width - 16.0, ITEM_HEIGHT - 12.0),
                radius: 5.0,
                color: SURFACE_RAISED,
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
                &mut resources,
                category,
                10.0,
                if item.failed { ERROR } else { ACCENT },
                18.0,
                y + 11.0,
                state.width - 36.0,
            )?;
            add_text(
                state.pane,
                &mut packet,
                &mut resources,
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
                &mut resources,
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
            &mut resources,
            &one_line(&state.error, 80),
            10.0,
            ERROR,
            12.0,
            state.height - 20.0,
            state.width - 24.0,
        )?;
    }
    let bytes = packet
        .encode()
        .map_err(|error| format!("encode inbox draw packet: {error}"))?;
    host::graphics_submit_draw_packet(state.pane, &bytes)
        .map_err(|error| format!("submit inbox draw packet: {error:?}"))?;
    state.text_resources = resources;
    state.error.clear();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_text(
    pane: u64,
    packet: &mut Packet,
    resources: &mut Vec<u64>,
    text: &str,
    size: f32,
    color: Color,
    x: f32,
    y: f32,
    width: f32,
) -> Result<(), String> {
    let layout = host::graphics_layout_text(
        pane,
        text,
        host::TextLayoutOptions {
            font_intent: host::FontIntent::Ui,
            font_weight: host::FontWeight::Normal,
            font_style: host::FontStyle::Normal,
            font_size: size,
            max_width: width.max(1.0),
            line_height: size * 1.3,
            red: color.r,
            green: color.g,
            blue: color.b,
            alpha: color.a,
        },
    )
    .map_err(|error| format!("layout inbox text: {error:?}"))?;
    resources.push(layout.handle);
    packet.push(DrawCommand::DrawTextLayout {
        layout: layout.handle,
        x,
        y,
    });
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

fn one_line(text: &str, maximum: usize) -> String {
    text.replace(['\r', '\n'], " ")
        .chars()
        .take(maximum)
        .collect()
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
    fn show_action_focuses_the_global_inbox_pane() {
        assert!(focus_pane_command(0).is_empty());
        let commands = focus_pane_command(42);
        assert!(matches!(
            commands.as_slice(),
            [Command::SendHostMessage(body)]
                if body == br#"{"type":"focus-pane","pane":42}"#
        ));
    }
}

export!(InboxPlugin);
