use brood_graphics::theme::ThemeSnapshot;
use std::{collections::VecDeque, sync::Mutex};

use brood_graphics::{Color, Command as DrawCommand, Packet, Rect};
#[cfg(not(feature = "pure-renderer"))]
use brood_plugin_sdk::bindings;
#[cfg(not(feature = "pure-renderer"))]
use brood_plugin_sdk::graphics::{UiFrame, reuse_pane_content};
#[cfg(feature = "pure-renderer")]
use brood_plugin_sdk::{Facets, export_visual_plugin, facet_codec, facets};
use brood_plugin_sdk::{
    exports::brood::plugin::actor::{
        Command, Event, Guest, LifecycleEvent, LocalActorMessage, PresentationEvent,
    },
    input::BUTTON_PRIMARY as LEFT_BUTTON,
    text::one_line,
};
use serde::{Deserialize, Serialize};

const INIT_ACTOR_ID: &str = "brood.client-init";
const SHELL_ACTOR_ID: &str = "session-switcher";
const INBOX_ACTOR_ID: &str = "inbox";
const HEADER_HEIGHT: f32 = 48.0;
const ITEM_HEIGHT: f32 = 72.0;
const MAX_ITEMS: usize = 32;
const MAX_TITLE_CHARS: usize = 96;
const MAX_DETAIL_CHARS: usize = 160;
const CLOSE_TARGET: usize = usize::MAX;
const SHOW_MESSAGE: &[u8] = br#"{"kind":"inbox-action","action":"show"}"#;
const TOGGLE_MESSAGE: &[u8] = br#"{"kind":"inbox-action","action":"toggle"}"#;

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

#[derive(Clone, Deserialize, Serialize)]
struct InboxItem {
    id: u64,
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
    #[cfg(not(feature = "pure-renderer"))]
    text_resources: Vec<u64>,
    error: String,
    hovered: Option<usize>,
    pressed: Option<usize>,
    selected: Option<usize>,
    next_item_id: u64,
    #[cfg(feature = "pure-renderer")]
    render_revision: u64,
    #[cfg(feature = "pure-renderer")]
    published_view: Vec<u8>,
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
            #[cfg(not(feature = "pure-renderer"))]
            text_resources: Vec::new(),
            error: String::new(),
            hovered: None,
            pressed: None,
            selected: None,
            next_item_id: 1,
            #[cfg(feature = "pure-renderer")]
            render_revision: 0,
            #[cfg(feature = "pure-renderer")]
            published_view: Vec::new(),
        }
    }
}

static STATE: Mutex<InboxState> = Mutex::new(InboxState::new());
#[cfg(feature = "pure-renderer")]
static PENDING_PUBLICATION: Mutex<Option<(u64, Vec<u8>)>> = Mutex::new(None);

#[cfg(feature = "pure-renderer")]
#[derive(Deserialize, Serialize)]
struct InboxRenderView {
    items: VecDeque<InboxItem>,
    scroll: usize,
    selected_id: Option<u64>,
    error: String,
}
#[cfg(feature = "pure-renderer")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum InboxTarget {
    Close,
    Item(u64),
}
#[cfg(feature = "pure-renderer")]
#[derive(Clone, Default, Deserialize, Serialize)]
struct InteractionState {
    hovered: Option<InboxTarget>,
    pressed: Option<InboxTarget>,
    selected: Option<u64>,
}
#[cfg(feature = "pure-renderer")]
#[derive(Deserialize, Serialize)]
struct InteractionRegion {
    rect: [f32; 4],
    target: InboxTarget,
}
#[cfg(feature = "pure-renderer")]
#[derive(Deserialize, Serialize)]
struct InteractionModel {
    regions: Vec<InteractionRegion>,
    item_ids: Vec<u64>,
    selected_id: Option<u64>,
}
#[cfg(feature = "pure-renderer")]
#[derive(Deserialize, Serialize)]
enum FacetIntent {
    Activate(InboxTarget),
    Select(u64),
    Delete(u64),
    Scroll(i8),
    Close,
}

#[cfg(feature = "pure-renderer")]
struct PublicationTurn;
#[cfg(feature = "pure-renderer")]
impl Drop for PublicationTurn {
    fn drop(&mut self) {
        let mut pending = PENDING_PUBLICATION
            .lock()
            .expect("publication mutex poisoned");
        if let Some((revision, bytes)) = pending.take()
            && brood_plugin_sdk::publication::publish(revision, &bytes).is_err()
        {
            // Retain the newest staged view for the next actor turn.
            *pending = Some((revision, bytes));
        }
    }
}

struct InboxPlugin;

impl Guest for InboxPlugin {
    fn dispatch(event: Event) -> Vec<Command> {
        #[cfg(feature = "pure-renderer")]
        let _publication_turn = PublicationTurn;
        match event {
            #[cfg(feature = "pure-renderer")]
            Event::FacetInput(batch) => handle_facet_input(batch.events),
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
    if sender == "key-policy" && (body == SHOW_MESSAGE || body == TOGGLE_MESSAGE) {
        let Ok(state) = STATE.lock() else {
            return Vec::new();
        };
        return toggle_pane_commands(state.pane);
    }
    let Some(item) = parse_item(sender, body) else {
        return Vec::new();
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let Some(next_item_id) = state.next_item_id.checked_add(1) else {
        state.error = "Inbox notification identity limit reached".to_string();
        redraw_or_record_error(&mut state);
        return Vec::new();
    };
    let item = InboxItem {
        id: state.next_item_id,
        ..item
    };
    state.next_item_id = next_item_id;
    push_item(&mut state.items, item);
    state.scroll = 0;
    state.hovered = None;
    state.pressed = None;
    state.selected = (!state.items.is_empty()).then_some(0);
    redraw_or_record_error(&mut state);
    Vec::new()
}

fn toggle_pane_commands(pane: u64) -> Vec<Command> {
    if pane == 0 {
        return Vec::new();
    }
    vec![Command::SendLocalMessage(LocalActorMessage {
        actor: SHELL_ACTOR_ID.to_string(),
        body: format!(r#"{{"kind":"inbox-toggle","pane":{pane}}}"#).into_bytes(),
    })]
}

fn hide_pane_commands(pane: u64) -> Vec<Command> {
    (pane != 0)
        .then(|| {
            Command::SendLocalMessage(LocalActorMessage {
                actor: SHELL_ACTOR_ID.to_string(),
                body: format!(r#"{{"kind":"inbox-hide","pane":{pane}}}"#).into_bytes(),
            })
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
        id: 0,
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
            clamp_selection(&mut state);
            clamp_scroll(&mut state);
            paint_for_resize_or_record_error(&mut state, paint);
            Vec::new()
        }
        #[cfg(not(feature = "pure-renderer"))]
        PresentationEvent::PointerMoved(moved) if moved.pane == state.pane => {
            let hovered = if close_target_at(&state, moved.x, moved.y) {
                Some(CLOSE_TARGET)
            } else {
                inbox_target_at(&state, moved.y)
            };
            if state.hovered != hovered {
                state.hovered = hovered;
                redraw_or_record_error(&mut state);
            }
            Vec::new()
        }
        #[cfg(not(feature = "pure-renderer"))]
        PresentationEvent::PointerLeft(left) if left.pane == state.pane => {
            if state.hovered.take().is_some() || state.pressed.take().is_some() {
                redraw_or_record_error(&mut state);
            }
            Vec::new()
        }
        #[cfg(not(feature = "pure-renderer"))]
        PresentationEvent::PointerButton(button)
            if button.pane == state.pane && button.button == LEFT_BUTTON =>
        {
            if close_target_at(&state, button.x, button.y) {
                state.hovered = Some(CLOSE_TARGET);
                let activate = !button.pressed && state.pressed == Some(CLOSE_TARGET);
                state.pressed = button.pressed.then_some(CLOSE_TARGET);
                redraw_or_record_error(&mut state);
                return if button.pressed {
                    Vec::new()
                } else if activate {
                    hide_pane_commands(state.pane)
                } else {
                    Vec::new()
                };
            }
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
                    clamp_selection(&mut state);
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
        #[cfg(not(feature = "pure-renderer"))]
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
        #[cfg(not(feature = "pure-renderer"))]
        PresentationEvent::Key(key) if key.pane == state.pane && key.pressed => {
            let commands = match key.logical_key.as_str() {
                "Escape" => hide_pane_commands(state.pane),
                "ArrowDown" | "j" => {
                    if !state.items.is_empty() {
                        state.selected = Some(
                            state
                                .selected
                                .unwrap_or(0)
                                .saturating_add(1)
                                .min(state.items.len() - 1),
                        );
                        reveal_selection(&mut state);
                    }
                    Vec::new()
                }
                "ArrowUp" | "k" => {
                    if !state.items.is_empty() {
                        state.selected = Some(state.selected.unwrap_or(0).saturating_sub(1));
                        reveal_selection(&mut state);
                    }
                    Vec::new()
                }
                "Enter" => activate_selected(&mut state),
                "Delete" | "Backspace" => {
                    if let Some(selected) = state.selected
                        && selected < state.items.len()
                    {
                        state.items.remove(selected);
                        clamp_selection(&mut state);
                        clamp_scroll(&mut state);
                    }
                    Vec::new()
                }
                _ => Vec::new(),
            };
            redraw_or_record_error(&mut state);
            commands
        }
        #[cfg(not(feature = "pure-renderer"))]
        PresentationEvent::RenderHint(hint) => {
            if let Err(error) =
                brood_plugin_sdk::graphics::render_with_hint(&hint, || redraw(&mut state, None))
            {
                state.error = error;
            }
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

#[cfg(any(test, not(feature = "pure-renderer")))]
#[allow(
    dead_code,
    reason = "legacy hit testing is compiled by feature test targets"
)]
fn inbox_target_at(state: &InboxState, y: f32) -> Option<usize> {
    if !y.is_finite() || y < HEADER_HEIGHT || y >= state.height {
        return None;
    }
    let row = ((y - HEADER_HEIGHT) / ITEM_HEIGHT).floor() as usize;
    (row < visible_item_count(state))
        .then_some(state.scroll + row)
        .filter(|index| *index < state.items.len())
}

#[cfg(any(test, not(feature = "pure-renderer")))]
fn close_target_at(state: &InboxState, x: f32, y: f32) -> bool {
    x.is_finite()
        && y.is_finite()
        && x >= state.width - 42.0
        && x < state.width
        && (0.0..HEADER_HEIGHT).contains(&y)
}

fn clamp_scroll(state: &mut InboxState) {
    state.scroll = state
        .scroll
        .min(state.items.len().saturating_sub(visible_item_count(state)));
}

fn clamp_selection(state: &mut InboxState) {
    state.selected = if state.items.is_empty() {
        None
    } else {
        Some(state.selected.unwrap_or(0).min(state.items.len() - 1))
    };
}

fn reveal_selection(state: &mut InboxState) {
    let Some(selected) = state.selected else {
        return;
    };
    let visible = visible_item_count(state);
    if selected < state.scroll {
        state.scroll = selected;
    } else if selected >= state.scroll + visible {
        state.scroll = selected + 1 - visible;
    }
}

#[cfg(any(test, not(feature = "pure-renderer")))]
fn activate_selected(state: &mut InboxState) -> Vec<Command> {
    let Some(selected) = state.selected else {
        return Vec::new();
    };
    let Some(item) = state.items.remove(selected) else {
        return Vec::new();
    };
    clamp_selection(state);
    clamp_scroll(state);
    vec![Command::SendLocalMessage(LocalActorMessage {
        actor: item.target_actor,
        body: br#"{"kind":"focus"}"#.to_vec(),
    })]
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

#[allow(
    clippy::needless_return,
    reason = "the return excludes the separately compiled legacy rendering branch"
)]
fn paint_for_resize_or_record_error(state: &mut InboxState, paint: ResizePaintPlan) {
    #[cfg(feature = "pure-renderer")]
    {
        let _ = paint;
        if let Err(error) = publish_view(state) {
            state.error = error;
        }
        return;
    }
    #[cfg(not(feature = "pure-renderer"))]
    if !brood_plugin_sdk::graphics::render_is_active() {
        let result = match paint {
            ResizePaintPlan::OrdinaryRedraw => brood_plugin_sdk::graphics::request_render(),
            ResizePaintPlan::TaggedRedraw(resize_epoch) => {
                brood_plugin_sdk::graphics::request_render_for_resize(
                    state.pane,
                    Some(resize_epoch),
                )
            }
            ResizePaintPlan::Reuse(resize_epoch) => reuse_pane_content(state.pane, resize_epoch),
        };
        if let Err(error) = result {
            state.error = error;
        }
        return;
    }
    #[cfg(not(feature = "pure-renderer"))]
    let result = match paint {
        ResizePaintPlan::OrdinaryRedraw => redraw(state, None),
        ResizePaintPlan::TaggedRedraw(resize_epoch) => redraw(state, Some(resize_epoch)),
        ResizePaintPlan::Reuse(resize_epoch) => reuse_pane_content(state.pane, resize_epoch)
            .or_else(|_reuse_error| redraw(state, Some(resize_epoch)).map(|_| ())),
    };
    #[cfg(not(feature = "pure-renderer"))]
    if let Err(error) = result {
        state.error = error;
    }
}

fn redraw_for_resize_or_record_error(state: &mut InboxState, resize_epoch: Option<u64>) {
    #[cfg(feature = "pure-renderer")]
    let result = {
        let _ = resize_epoch;
        publish_view(state)
    };
    #[cfg(not(feature = "pure-renderer"))]
    let result = if brood_plugin_sdk::graphics::render_is_active() {
        redraw(state, resize_epoch)
    } else {
        brood_plugin_sdk::graphics::request_render_for_resize(state.pane, resize_epoch)
    };
    if let Err(error) = result {
        state.error = error;
    }
}

#[cfg(feature = "pure-renderer")]
fn publish_view(state: &mut InboxState) -> Result<(), String> {
    let view = InboxRenderView {
        items: state.items.clone(),
        scroll: state.scroll,
        selected_id: state
            .selected
            .and_then(|i| state.items.get(i).map(|item| item.id)),
        error: state.error.clone(),
    };
    let bytes =
        serde_json::to_vec(&view).map_err(|error| format!("serialize inbox view: {error}"))?;
    if bytes == state.published_view {
        return Ok(());
    }
    state.render_revision = state.render_revision.saturating_add(1).max(1);
    state.published_view.clone_from(&bytes);
    *PENDING_PUBLICATION
        .lock()
        .map_err(|_| "inbox publication mutex poisoned".to_string())? =
        Some((state.render_revision, bytes));
    Ok(())
}

#[cfg(not(feature = "pure-renderer"))]
fn redraw(state: &mut InboxState, resize_epoch: Option<u64>) -> Result<(), String> {
    if state.pane == 0 {
        return Ok(());
    }
    let mut frame = UiFrame::new(state.pane);
    let packet = render_packet(&ThemeSnapshot::default(), state, &mut frame)?;
    frame.queue(resize_epoch, &packet, &mut state.text_resources)?;
    state.error.clear();
    Ok(())
}

fn render_packet(
    theme: &ThemeSnapshot,
    state: &InboxState,
    sink: &mut impl InboxTextSink,
) -> Result<Packet, String> {
    let mut packet = Packet::new();
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, state.height),
        color: theme.bg("ui.background"),
    });
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, HEADER_HEIGHT),
        color: theme.bg("ui.statusline"),
    });
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, HEADER_HEIGHT - 1.0, state.width, 1.0),
        color: theme.fg("ui.window"),
    });
    add_text(
        state.pane,
        &mut packet,
        sink,
        "Inbox",
        17.0,
        theme.fg("ui.text"),
        14.0,
        12.0,
        state.width - 80.0,
    )?;
    add_text(
        state.pane,
        &mut packet,
        sink,
        &state.items.len().to_string(),
        12.0,
        theme.fg("ui.text.focus"),
        state.width - 70.0,
        15.0,
        28.0,
    )?;
    add_text(
        state.pane,
        &mut packet,
        sink,
        "×",
        18.0,
        if state.pressed == Some(CLOSE_TARGET) {
            theme.fg("ui.text")
        } else if state.hovered == Some(CLOSE_TARGET) {
            theme.fg("ui.text.focus")
        } else {
            theme.fg("ui.text.inactive")
        },
        state.width - 30.0,
        11.0,
        18.0,
    )?;

    if state.items.is_empty() {
        add_text(
            state.pane,
            &mut packet,
            sink,
            "No notifications",
            14.0,
            theme.fg("ui.text.inactive"),
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
            let selected = state.selected == Some(absolute_index);
            packet.push(DrawCommand::FillRoundedRect {
                rect: Rect::new(8.0, y + 7.0, state.width - 16.0, ITEM_HEIGHT - 12.0),
                radius: 5.0,
                color: if pressed {
                    theme.bg("ui.selection")
                } else if hovered || selected {
                    theme.bg("ui.menu.selected")
                } else {
                    theme.bg("ui.menu")
                },
            });
            packet.push(DrawCommand::FillRect {
                rect: Rect::new(8.0, y + 7.0, 3.0, ITEM_HEIGHT - 12.0),
                color: if item.failed {
                    theme.fg("error")
                } else {
                    theme.fg("info")
                },
            });
            let category = if item.category.is_empty() {
                "update"
            } else {
                &item.category
            };
            add_text(
                state.pane,
                &mut packet,
                sink,
                category,
                10.0,
                if item.failed {
                    theme.fg("error")
                } else {
                    theme.fg("ui.text.focus")
                },
                18.0,
                y + 11.0,
                state.width - 36.0,
            )?;
            if hovered || pressed || selected {
                add_text(
                    state.pane,
                    &mut packet,
                    sink,
                    if pressed { "Opening…" } else { "Open  ›" },
                    10.0,
                    theme.fg("ui.text.focus"),
                    (state.width - 74.0).max(18.0),
                    y + 11.0,
                    58.0,
                )?;
            }
            add_text(
                state.pane,
                &mut packet,
                sink,
                &truncate(&item.title, state.width, 13.0),
                13.0,
                theme.fg(if hovered || selected {
                    "ui.menu.selected"
                } else {
                    "ui.text"
                }),
                18.0,
                y + 27.0,
                state.width - 36.0,
            )?;
            add_text(
                state.pane,
                &mut packet,
                sink,
                &truncate(&item.detail, state.width, 11.0),
                11.0,
                theme.fg("ui.text.inactive"),
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
            sink,
            &one_line(&state.error, 80),
            10.0,
            theme.fg("error"),
            12.0,
            state.height - 20.0,
            state.width - 24.0,
        )?;
    }
    Ok(packet)
}

trait InboxTextSink {
    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        packet: &mut Packet,
        text: &str,
        size: f32,
        color: Color,
        x: f32,
        y: f32,
        width: f32,
    ) -> Result<(), String>;
}

#[cfg(not(feature = "pure-renderer"))]
impl InboxTextSink for UiFrame {
    fn draw_text(
        &mut self,
        packet: &mut Packet,
        text: &str,
        size: f32,
        color: Color,
        x: f32,
        y: f32,
        width: f32,
    ) -> Result<(), String> {
        self.draw_text_clipped(packet, text, size, width, size * 1.3, color, x, y)
            .map(|_| ())
            .map_err(|error| format!("layout inbox text: {error}"))
    }
}

#[cfg(feature = "pure-renderer")]
impl InboxTextSink for brood_plugin_sdk::graphics::TextLayoutCache {
    fn draw_text(
        &mut self,
        packet: &mut Packet,
        text: &str,
        size: f32,
        color: Color,
        x: f32,
        y: f32,
        width: f32,
    ) -> Result<(), String> {
        self.draw_text_clipped(packet, text, size, width, size * 1.3, color, x, y)
            .map(|_| ())
            .map_err(|error| format!("layout inbox text: {error}"))
    }
}

#[allow(clippy::too_many_arguments)]
fn add_text(
    _pane: u64,
    packet: &mut Packet,
    frame: &mut impl InboxTextSink,
    text: &str,
    size: f32,
    color: Color,
    x: f32,
    y: f32,
    width: f32,
) -> Result<(), String> {
    frame
        .draw_text(packet, text, size, color, x, y, width)
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

#[cfg(feature = "pure-renderer")]
fn handle_facet_input(events: Vec<Vec<u8>>) -> Vec<Command> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let mut commands = Vec::new();
    for bytes in events {
        let Ok(intent) = serde_json::from_slice(&bytes) else {
            state.error = "Invalid inbox facet intent".to_string();
            continue;
        };
        match intent {
            FacetIntent::Activate(InboxTarget::Close) | FacetIntent::Close => {
                commands.extend(hide_pane_commands(state.pane));
            }
            FacetIntent::Activate(InboxTarget::Item(id)) => {
                if let Some(index) = item_index(&state, id)
                    && let Some(item) = state.items.remove(index)
                {
                    commands.push(Command::SendLocalMessage(LocalActorMessage {
                        actor: item.target_actor,
                        body: br#"{"kind":"focus"}"#.to_vec(),
                    }));
                    clamp_selection(&mut state);
                    clamp_scroll(&mut state);
                }
            }
            FacetIntent::Select(id) => {
                if let Some(index) = item_index(&state, id) {
                    state.selected = Some(index);
                    reveal_selection(&mut state);
                }
            }
            FacetIntent::Delete(id) => {
                if let Some(index) = item_index(&state, id) {
                    state.items.remove(index);
                    clamp_selection(&mut state);
                    clamp_scroll(&mut state);
                }
            }
            FacetIntent::Scroll(direction) => {
                let maximum = state.items.len().saturating_sub(visible_item_count(&state));
                if direction > 0 {
                    state.scroll = (state.scroll + 1).min(maximum);
                } else if direction < 0 {
                    state.scroll = state.scroll.saturating_sub(1);
                }
            }
        }
    }
    redraw_or_record_error(&mut state);
    commands
}

#[cfg(feature = "pure-renderer")]
fn item_index(state: &InboxState, id: u64) -> Option<usize> {
    state.items.iter().position(|item| item.id == id)
}

#[cfg(feature = "pure-renderer")]
impl Facets for InboxPlugin {
    fn render(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_render_request(
            &input,
            facet_codec::DecodeLimits {
                max_wire_bytes: input.len(),
                max_allocated_bytes: input.len().saturating_mul(4).max(4096),
                max_allocations: 4096,
            },
        )
        .expect("valid inbox render request");
        let theme = ThemeSnapshot::decode(&request.configuration).expect("valid theme snapshot");
        let view: InboxRenderView =
            serde_json::from_slice(&request.application_view).expect("valid inbox view");
        let sequence: InteractionState =
            serde_json::from_slice(&request.sequence_state).unwrap_or_default();
        let panes = request
            .panes
            .iter()
            .map(|pane| {
                let mut state = InboxState::new();
                state.pane = pane.pane;
                state.width = pane.logical_width as f32;
                state.height = pane.logical_height as f32;
                state.items = view.items.clone();
                state.scroll = view.scroll;
                state.error.clone_from(&view.error);
                state.selected = view
                    .selected_id
                    .and_then(|id| state.items.iter().position(|item| item.id == id));
                state.hovered = sequence.hovered.as_ref().and_then(|target| match target {
                    InboxTarget::Close => Some(CLOSE_TARGET),
                    InboxTarget::Item(id) => state.items.iter().position(|item| item.id == *id),
                });
                state.pressed = sequence.pressed.as_ref().and_then(|target| match target {
                    InboxTarget::Close => Some(CLOSE_TARGET),
                    InboxTarget::Item(id) => state.items.iter().position(|item| item.id == *id),
                });
                let mut sink = brood_plugin_sdk::graphics::TextLayoutCache::new(pane.pane);
                sink.begin_frame();
                let packet =
                    render_packet(&theme, &state, &mut sink).expect("bounded inbox render");
                let mut regions = vec![InteractionRegion {
                    rect: [state.width - 42.0, 0.0, 42.0, HEADER_HEIGHT],
                    target: InboxTarget::Close,
                }];
                for (row, item) in state
                    .items
                    .iter()
                    .skip(state.scroll)
                    .take(visible_item_count(&state))
                    .enumerate()
                {
                    regions.push(InteractionRegion {
                        rect: [
                            0.0,
                            HEADER_HEIGHT + row as f32 * ITEM_HEIGHT,
                            state.width,
                            ITEM_HEIGHT,
                        ],
                        target: InboxTarget::Item(item.id),
                    });
                }
                let interaction = InteractionModel {
                    regions,
                    item_ids: state.items.iter().map(|item| item.id).collect(),
                    selected_id: view.selected_id,
                };
                facets::PaneOutput {
                    pane: pane.pane,
                    content: facets::PaneContent::Draw {
                        packet: packet.encode().expect("inbox packet"),
                        interaction: serde_json::to_vec(&interaction).expect("inbox interactions"),
                    },
                }
            })
            .collect();
        let response = facets::RenderResponse {
            token: request.token,
            panes,
            next_frame_ns: None,
        };
        facet_codec::encode_render_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded inbox response")
    }
    fn interpret(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_input_request(
            &input,
            facet_codec::DecodeLimits {
                max_wire_bytes: input.len(),
                max_allocated_bytes: input.len().saturating_mul(4).max(4096),
                max_allocations: 1024,
            },
        )
        .expect("valid inbox input");
        let interaction: InteractionModel =
            serde_json::from_slice(&request.interaction).expect("inbox interactions");
        let mut sequence: InteractionState =
            serde_json::from_slice(&request.sequence_state).unwrap_or_default();
        let point = match &request.event {
            facets::InputEvent::PointerMotion { x, y }
            | facets::InputEvent::PointerButton { x, y, .. }
            | facets::InputEvent::PointerScroll { x, y, .. } => Some((*x as f32, *y as f32)),
            _ => None,
        };
        let target = point.and_then(|(x, y)| {
            interaction
                .regions
                .iter()
                .find(|r| {
                    x >= r.rect[0]
                        && x < r.rect[0] + r.rect[2]
                        && y >= r.rect[1]
                        && y < r.rect[1] + r.rect[3]
                })
                .map(|r| r.target.clone())
        });
        let mut intents = Vec::new();
        match &request.event {
            facets::InputEvent::PointerMotion { .. } => sequence.hovered.clone_from(&target),
            facets::InputEvent::PointerButton {
                button,
                pressed: true,
                ..
            } if *button == LEFT_BUTTON => {
                sequence.hovered.clone_from(&target);
                sequence.pressed.clone_from(&target)
            }
            facets::InputEvent::PointerButton {
                button,
                pressed: false,
                ..
            } if *button == LEFT_BUTTON => {
                sequence.hovered.clone_from(&target);
                if sequence.pressed == target
                    && let Some(target) = target.clone()
                {
                    intents.push(FacetIntent::Activate(target));
                }
                sequence.pressed = None;
            }
            facets::InputEvent::PointerLeave
            | facets::InputEvent::FocusLost
            | facets::InputEvent::CaptureLost => {
                sequence.hovered = None;
                sequence.pressed = None
            }
            facets::InputEvent::PointerScroll { vertical, .. } if *vertical != 0.0 => {
                intents.push(FacetIntent::Scroll(if *vertical < 0.0 { 1 } else { -1 }));
            }
            facets::InputEvent::Key {
                logical_key,
                pressed: true,
                ..
            } => {
                let selected = sequence
                    .selected
                    .filter(|id| interaction.item_ids.contains(id))
                    .or(interaction.selected_id);
                match logical_key.as_str() {
                    "Escape" => intents.push(FacetIntent::Close),
                    "ArrowDown" | "j" => {
                        if let Some(id) = adjacent_id(&interaction.item_ids, selected, 1) {
                            sequence.selected = Some(id);
                            intents.push(FacetIntent::Select(id));
                        }
                    }
                    "ArrowUp" | "k" => {
                        if let Some(id) = adjacent_id(&interaction.item_ids, selected, -1) {
                            sequence.selected = Some(id);
                            intents.push(FacetIntent::Select(id));
                        }
                    }
                    "Home" => {
                        if let Some(id) = interaction.item_ids.first().copied() {
                            sequence.selected = Some(id);
                            intents.push(FacetIntent::Select(id));
                        }
                    }
                    "End" => {
                        if let Some(id) = interaction.item_ids.last().copied() {
                            sequence.selected = Some(id);
                            intents.push(FacetIntent::Select(id));
                        }
                    }
                    "Enter" => {
                        if let Some(id) = selected.filter(|id| interaction.item_ids.contains(id)) {
                            intents.push(FacetIntent::Activate(InboxTarget::Item(id)));
                        }
                    }
                    "Delete" | "Backspace" => {
                        if let Some(id) = selected.filter(|id| interaction.item_ids.contains(id)) {
                            intents.push(FacetIntent::Delete(id));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        let events = intents
            .into_iter()
            .map(|intent| serde_json::to_vec(&intent).expect("inbox intent"))
            .collect();
        let next_sequence_state = serde_json::to_vec(&sequence).expect("inbox sequence");
        let response = facets::InputResponse {
            token: request.token,
            next_sequence_state,
            events,
            requests: vec![facets::HostRequest::Render],
        };
        facet_codec::encode_input_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded inbox input response")
    }
}

#[cfg(feature = "pure-renderer")]
fn adjacent_id(ids: &[u64], selected: Option<u64>, direction: i8) -> Option<u64> {
    if ids.is_empty() {
        return None;
    }
    let index = selected
        .and_then(|id| ids.iter().position(|candidate| *candidate == id))
        .unwrap_or(0);
    let next = if direction > 0 {
        index.saturating_add(1).min(ids.len() - 1)
    } else {
        index.saturating_sub(1)
    };
    ids.get(next).copied()
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

    #[cfg(feature = "pure-renderer")]
    fn interpret(
        event: brood_plugin_sdk::facets::InputEvent,
        sequence_state: Vec<u8>,
    ) -> brood_plugin_sdk::facets::InputResponse {
        use brood_plugin_sdk::{Facets, facet_codec, facets};
        let limits = facets::InvocationLimits {
            max_output_bytes: 4096,
            max_packet_bytes: 4096,
            max_interaction_bytes: 4096,
            max_sequence_bytes: 512,
            max_events: 4,
            max_event_bytes: 1024,
            max_host_requests: 2,
            max_resources: 8,
            max_resource_bytes: 65536,
        };
        let token = facets::InputToken {
            scope: facets::Scope {
                client_generation: 1,
                actor_generation: 2,
                facet_generation: 3,
            },
            input_sequence: 4,
            frame: 5,
            pane: 9,
            snapshot_revision: 6,
            sequence_revision: 0,
        };
        let interaction = serde_json::to_vec(&super::InteractionModel {
            regions: vec![super::InteractionRegion {
                rect: [0.0, 48.0, 320.0, 72.0],
                target: super::InboxTarget::Item(77),
            }],
            item_ids: vec![77, 91],
            selected_id: Some(77),
        })
        .expect("regions");
        let request = facets::InputRequest {
            token,
            geometry: facets::PaneTarget {
                pane: 9,
                logical_width: 320.0,
                logical_height: 480.0,
                physical_width: 320,
                physical_height: 480,
                scale: 1.0,
                clip: [0.0, 0.0, 320.0, 480.0],
                visible: true,
                focused: true,
            },
            event,
            interaction,
            sequence_state,
            limits,
        };
        let mut wire = vec![0; 4096];
        let len = facet_codec::encode_input_request(&request, &mut wire).expect("encode");
        wire.truncate(len);
        let output = <super::InboxPlugin as Facets>::interpret(wire);
        facet_codec::decode_input_response(
            &output,
            facet_codec::DecodeLimits {
                max_wire_bytes: output.len(),
                max_allocated_bytes: 8192,
                max_allocations: 64,
            },
        )
        .expect("decode")
    }

    #[cfg(feature = "pure-renderer")]
    #[test]
    fn facet_press_release_activates_stable_id_and_retry_is_deterministic() {
        use brood_plugin_sdk::facets::InputEvent;
        let press = interpret(
            InputEvent::PointerButton {
                x: 10.0,
                y: 50.0,
                button: LEFT_BUTTON,
                pressed: true,
                modifiers: 0,
            },
            Vec::new(),
        );
        assert!(press.events.is_empty());
        let release_event = InputEvent::PointerButton {
            x: 10.0,
            y: 50.0,
            button: LEFT_BUTTON,
            pressed: false,
            modifiers: 0,
        };
        let first = interpret(release_event.clone(), press.next_sequence_state.clone());
        let retry = interpret(release_event, press.next_sequence_state);
        assert_eq!(first.events, retry.events);
        assert_eq!(first.next_sequence_state, retry.next_sequence_state);
        let intent: super::FacetIntent = serde_json::from_slice(&first.events[0]).expect("intent");
        assert!(matches!(
            intent,
            super::FacetIntent::Activate(super::InboxTarget::Item(77))
        ));
    }

    #[cfg(feature = "pure-renderer")]
    #[test]
    fn facet_keyboard_and_scroll_emit_stable_semantics() {
        use brood_plugin_sdk::facets::InputEvent;
        let down = interpret(
            InputEvent::Key {
                logical_key: "ArrowDown".to_string(),
                code: 0,
                pressed: true,
                repeat: false,
                modifiers: 0,
            },
            Vec::new(),
        );
        let intent: super::FacetIntent =
            serde_json::from_slice(&down.events[0]).expect("select intent");
        assert!(matches!(intent, super::FacetIntent::Select(91)));
        let scroll = interpret(
            InputEvent::PointerScroll {
                x: 10.0,
                y: 50.0,
                horizontal: 0.0,
                vertical: -1.0,
                modifiers: 0,
            },
            Vec::new(),
        );
        let intent: super::FacetIntent =
            serde_json::from_slice(&scroll.events[0]).expect("scroll intent");
        assert!(matches!(intent, super::FacetIntent::Scroll(1)));
    }

    #[cfg(feature = "pure-renderer")]
    #[test]
    fn live_lookup_does_not_retarget_after_reorder_or_removal() {
        let mut state = InboxState::new();
        for id in [10, 20, 30] {
            state.items.push_back(InboxItem {
                id,
                category: String::new(),
                title: id.to_string(),
                detail: String::new(),
                target_actor: format!("actor-{id}"),
                failed: false,
            });
        }
        state.items.swap(0, 2);
        assert_eq!(item_index(&state, 20), Some(1));
        state.items.remove(1);
        assert_eq!(item_index(&state, 20), None);
        assert_eq!(item_index(&state, 10), Some(1));
    }

    #[cfg(feature = "pure-renderer")]
    #[test]
    fn unchanged_view_does_not_advance_publication_revision() {
        let mut state = InboxState::new();
        publish_view(&mut state).expect("initial view");
        let revision = state.render_revision;
        PENDING_PUBLICATION.lock().expect("pending").take();
        publish_view(&mut state).expect("unchanged view");
        assert_eq!(state.render_revision, revision);
        assert!(PENDING_PUBLICATION.lock().expect("pending").is_none());
        state.error = "changed".to_string();
        publish_view(&mut state).expect("changed view");
        assert_eq!(state.render_revision, revision + 1);
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
                    id: index as u64 + 1,
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
    fn toggle_action_asks_the_root_shell_to_toggle_the_global_inbox_pane() {
        assert!(toggle_pane_commands(0).is_empty());
        let commands = toggle_pane_commands(42);
        assert!(matches!(
            commands.as_slice(),
            [Command::SendLocalMessage(LocalActorMessage { actor, body })]
                if actor == SHELL_ACTOR_ID
                    && body == br#"{"kind":"inbox-toggle","pane":42}"#
        ));
    }

    #[test]
    fn keyboard_activation_removes_the_selected_item_and_focuses_its_actor() {
        let mut state = InboxState::new();
        push_item(
            &mut state.items,
            InboxItem {
                id: 1,
                category: "agent".into(),
                title: "Done".into(),
                detail: String::new(),
                target_actor: "agent-ui".into(),
                failed: false,
            },
        );
        state.selected = Some(0);
        let commands = activate_selected(&mut state);
        assert!(state.items.is_empty());
        assert_eq!(state.selected, None);
        assert!(matches!(
            commands.as_slice(),
            [Command::SendLocalMessage(LocalActorMessage { actor, body })]
                if actor == "agent-ui" && body == br#"{"kind":"focus"}"#
        ));
    }

    #[test]
    fn close_target_is_bounded_to_the_header_control() {
        let state = InboxState::new();
        assert!(close_target_at(&state, 300.0, 20.0));
        assert!(!close_target_at(&state, 277.0, 20.0));
        assert!(!close_target_at(&state, 300.0, HEADER_HEIGHT));
        assert!(!close_target_at(&state, f32::NAN, 20.0));
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

#[cfg(feature = "pure-renderer")]
export_visual_plugin!(InboxPlugin);
#[cfg(not(feature = "pure-renderer"))]
bindings::export!(InboxPlugin with_types_in bindings);
