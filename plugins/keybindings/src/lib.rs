use std::{collections::BTreeMap, sync::Mutex};

use brood_graphics::{Color, Command, Packet, Rect, Transform, theme::ashen};
use brood_keybindings::{ACTIONS, Action, Binding, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_SUPER};
use brood_plugin_sdk::{
    actor::send_local,
    bindings,
    exports::brood::plugin::actor::{
        Command as ActorCommand, Event, Guest, LifecycleEvent, PresentationEvent,
    },
    graphics::{drop_resources, layout_ui_text, submit_packet_for_resize},
    input::BUTTON_PRIMARY as LEFT_BUTTON,
    text::truncate_to_width,
};
use serde::{Deserialize, Serialize};

const LAYOUT_ACTOR_ID: &str = "session-switcher";
const HEADER_HEIGHT: f32 = 64.0;
const FOOTER_HEIGHT: f32 = 32.0;
const ROW_HEIGHT: f32 = 42.0;
const SIDE_MARGIN: f32 = 16.0;
const MAX_BINDING_WIDTH: f32 = 190.0;
const RADIUS: f32 = 9.0;
const WINDOW_WIDTH: f32 = 820.0;
const WINDOW_HEIGHT: f32 = 600.0;
const WINDOW_MARGIN: f32 = 16.0;

const BG: Color = ashen::BACKGROUND;
const PANEL: Color = ashen::SURFACE;
const PANEL_RAISED: Color = ashen::SURFACE_RAISED;
const BORDER: Color = ashen::BORDER;
const TEXT: Color = ashen::TEXT_STRONG;
const FG: Color = ashen::TEXT;
const MUTED: Color = ashen::MUTED;
const ACCENT: Color = ashen::ACCENT;
const ERROR: Color = ashen::ERROR;
const SCRIM: Color = Color::rgba(0.0, 0.0, 0.0, 0.48);
const SHADOW_WIDE: Color = Color::rgba(0.0, 0.0, 0.0, 0.18);
const SHADOW_NEAR: Color = Color::rgba(0.0, 0.0, 0.0, 0.30);

#[derive(Deserialize)]
#[serde(tag = "type")]
enum KeybindingsMessage {
    #[serde(rename = "keybindings-show")]
    Show {
        #[serde(default)]
        bindings: Vec<BindingOverride>,
    },
    #[serde(rename = "keybindings-close")]
    Close,
    #[serde(rename = "keybinding-saved")]
    Saved {
        action: String,
        success: bool,
        #[serde(default)]
        message: String,
    },
}

#[derive(Clone, Deserialize)]
struct BindingOverride {
    action: String,
    key_code: i32,
    modifiers: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FloatingWindow {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl FloatingWindow {
    fn in_viewport(width: f32, height: f32) -> Self {
        let width = width.max(1.0);
        let height = height.max(1.0);
        let window_width = WINDOW_WIDTH.min((width - WINDOW_MARGIN * 2.0).max(1.0));
        let window_height = WINDOW_HEIGHT.min((height - WINDOW_MARGIN * 2.0).max(1.0));
        Self {
            x: (width - window_width) * 0.5,
            y: (height - window_height) * 0.5,
            width: window_width,
            height: window_height,
        }
    }

    fn contains(self, x: f32, y: f32) -> bool {
        x.is_finite()
            && y.is_finite()
            && x >= self.x
            && x < self.x + self.width
            && y >= self.y
            && y < self.y + self.height
    }
}

#[derive(Serialize)]
struct LayoutVisibility {
    #[serde(rename = "type")]
    message_type: &'static str,
    pane: u64,
    visible: bool,
}

struct KeybindingsState {
    pane: u64,
    width: f32,
    height: f32,
    visible: bool,
    query: String,
    selected: Option<usize>,
    scroll: usize,
    capture: Option<usize>,
    bindings: BTreeMap<String, Binding>,
    status: String,
    error: bool,
    hovered: Option<usize>,
    pressed: Option<usize>,
    backdrop_pressed: bool,
    escape_pending: bool,
    text_resources: Vec<u64>,
}

impl KeybindingsState {
    fn new() -> Self {
        Self {
            pane: 0,
            width: WINDOW_WIDTH + WINDOW_MARGIN * 2.0,
            height: WINDOW_HEIGHT + WINDOW_MARGIN * 2.0,
            visible: false,
            query: String::new(),
            selected: Some(0),
            scroll: 0,
            capture: None,
            bindings: default_bindings(),
            status: String::new(),
            error: false,
            hovered: None,
            pressed: None,
            backdrop_pressed: false,
            escape_pending: false,
            text_resources: Vec::new(),
        }
    }

    fn visible_rows(&self) -> usize {
        ((self.window().height - HEADER_HEIGHT - FOOTER_HEIGHT) / ROW_HEIGHT)
            .floor()
            .max(1.0) as usize
    }

    fn resize(&mut self, width: f32, height: f32) {
        self.width = width.max(1.0);
        self.height = height.max(1.0);
        self.hovered = None;
        self.pressed = None;
        self.backdrop_pressed = false;
        self.ensure_selected_visible();
    }

    fn window(&self) -> FloatingWindow {
        FloatingWindow::in_viewport(self.width, self.height)
    }

    fn filtered(&self) -> Vec<usize> {
        let mut matches = ACTIONS
            .iter()
            .enumerate()
            .filter_map(|(index, action)| {
                let binding = self.binding(action);
                let searchable = format!(
                    "{} {} {} {} {}",
                    action.title,
                    action.category,
                    action.id,
                    action.scope.label(),
                    binding_label(binding)
                );
                fuzzy_score(&self.query, &searchable).map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|(left_index, left_score), (right_index, right_score)| {
            right_score
                .cmp(left_score)
                .then_with(|| left_index.cmp(right_index))
        });
        matches.into_iter().map(|(index, _)| index).collect()
    }

    fn binding(&self, action: &Action) -> Binding {
        self.bindings
            .get(action.id)
            .copied()
            .unwrap_or(action.default_binding)
    }

    fn ensure_selection(&mut self) {
        let filtered = self.filtered();
        if self
            .selected
            .is_some_and(|selected| filtered.contains(&selected))
        {
            self.ensure_selected_visible();
            return;
        }
        self.selected = filtered.first().copied();
        self.scroll = 0;
    }

    fn ensure_selected_visible(&mut self) {
        let filtered = self.filtered();
        let Some(position) = self
            .selected
            .and_then(|selected| filtered.iter().position(|item| *item == selected))
        else {
            self.scroll = 0;
            return;
        };
        let rows = self.visible_rows();
        if position < self.scroll {
            self.scroll = position;
        } else if position >= self.scroll + rows {
            self.scroll = position + 1 - rows;
        }
    }
}

static STATE: Mutex<Option<KeybindingsState>> = Mutex::new(None);

struct KeybindingsActor;

impl Guest for KeybindingsActor {
    fn dispatch(event: Event) -> Vec<ActorCommand> {
        let Ok(mut slot) = STATE.lock() else {
            return Vec::new();
        };
        let state = slot.get_or_insert_with(KeybindingsState::new);
        match event {
            Event::Lifecycle(LifecycleEvent::Start(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Mailbox(message) => handle_message(state, &message.body),
            Event::Presentation(event) => handle_presentation(state, event),
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

fn handle_message(state: &mut KeybindingsState, bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok(message) = serde_json::from_slice::<KeybindingsMessage>(bytes) else {
        return Vec::new();
    };
    match message {
        KeybindingsMessage::Show { bindings } => {
            state.bindings = default_bindings();
            for binding in bindings {
                let candidate = Binding::new(binding.key_code, binding.modifiers);
                if candidate.is_valid() && ACTIONS.iter().any(|action| action.id == binding.action)
                {
                    state.bindings.insert(binding.action, candidate);
                }
            }
            state.visible = true;
            state.query.clear();
            state.capture = None;
            state.scroll = 0;
            state.status = "Select a shortcut to rebind".to_string();
            state.error = false;
            state.hovered = None;
            state.pressed = None;
            state.backdrop_pressed = false;
            state.escape_pending = false;
            state.ensure_selection();
            redraw_or_record_error(state);
            (state.pane != 0)
                .then(|| layout_visibility(state.pane, true))
                .into_iter()
                .collect()
        }
        KeybindingsMessage::Close => hide(state),
        KeybindingsMessage::Saved {
            action,
            success,
            message,
        } => {
            state.error = !success;
            state.status = if success {
                ACTIONS
                    .iter()
                    .find(|candidate| candidate.id == action)
                    .map_or_else(
                        || "Shortcut saved".to_string(),
                        |candidate| format!("Saved {}", candidate.title),
                    )
            } else if message.is_empty() {
                "Could not save shortcut".to_string()
            } else {
                message
            };
            redraw_or_record_error(state);
            Vec::new()
        }
    }
}

fn handle_presentation(
    state: &mut KeybindingsState,
    event: PresentationEvent,
) -> Vec<ActorCommand> {
    match event {
        PresentationEvent::PaneCreated(created) => {
            state.pane = created.pane;
            state.resize(created.width, created.height);
            redraw_or_record_error(state);
            vec![layout_visibility(state.pane, false)]
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            state.resize(resized.width, resized.height);
            redraw_for_resize_or_record_error(state, resized.resize_epoch);
            Vec::new()
        }
        PresentationEvent::Key(key) if key.pane == state.pane && !key.repeat => {
            if !key.pressed {
                if state.escape_pending && key.logical_key == "Escape" {
                    state.escape_pending = false;
                    return hide(state);
                }
                return Vec::new();
            }
            let commands = handle_key(
                state,
                key.physical_key,
                &key.logical_key,
                i32::try_from(key.modifiers).unwrap_or(i32::MAX),
            );
            redraw_or_record_error(state);
            commands
        }
        PresentationEvent::Text(text) if text.pane == state.pane && state.capture.is_none() => {
            state.query.extend(
                text.text
                    .chars()
                    .filter(|character| !character.is_control()),
            );
            state.scroll = 0;
            state.ensure_selection();
            redraw_or_record_error(state);
            Vec::new()
        }
        PresentationEvent::PointerMoved(moved) if moved.pane == state.pane => {
            let target = target_at(state, moved.x, moved.y);
            if state.hovered != target {
                state.hovered = target;
                redraw_or_record_error(state);
            }
            Vec::new()
        }
        PresentationEvent::PointerLeft(left) if left.pane == state.pane => {
            state.hovered = None;
            state.pressed = None;
            state.backdrop_pressed = false;
            redraw_or_record_error(state);
            Vec::new()
        }
        PresentationEvent::PointerButton(button)
            if button.pane == state.pane && button.button == LEFT_BUTTON =>
        {
            let window = state.window();
            let target = target_at(state, button.x, button.y);
            if button.pressed {
                state.backdrop_pressed = !window.contains(button.x, button.y);
                state.pressed = target;
                state.hovered = target;
                if let Some(target) = target {
                    state.selected = Some(target);
                }
            } else {
                if state.backdrop_pressed && !window.contains(button.x, button.y) {
                    state.backdrop_pressed = false;
                    return hide(state);
                }
                state.backdrop_pressed = false;
                let pressed = state.pressed.take();
                state.hovered = target;
                if pressed.is_some() && pressed == target {
                    state.capture = target;
                    state.status = "Press the new shortcut · Esc cancels".to_string();
                    state.error = false;
                }
            }
            redraw_or_record_error(state);
            Vec::new()
        }
        PresentationEvent::PointerScroll(scroll) if scroll.pane == state.pane => {
            if !state.window().contains(scroll.x, scroll.y) {
                return Vec::new();
            }
            let filtered = state.filtered();
            let max_scroll = filtered.len().saturating_sub(state.visible_rows());
            if scroll.vertical < 0.0 {
                state.scroll = (state.scroll + 1).min(max_scroll);
            } else if scroll.vertical > 0.0 {
                state.scroll = state.scroll.saturating_sub(1);
            }
            redraw_or_record_error(state);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn handle_key(
    state: &mut KeybindingsState,
    physical_key: u32,
    logical_key: &str,
    modifiers: i32,
) -> Vec<ActorCommand> {
    if let Some(target) = state.capture {
        if logical_key == "Escape" && modifiers == 0 {
            state.capture = None;
            state.status = "Rebinding cancelled".to_string();
            return Vec::new();
        }
        if is_modifier_key(logical_key) {
            return Vec::new();
        }
        let Ok(key_code) = i32::try_from(physical_key.saturating_add(8)) else {
            state.error = true;
            state.status = "This input source has no usable physical key".to_string();
            return Vec::new();
        };
        let binding = Binding::new(key_code, modifiers);
        if physical_key == 0 || !binding.is_valid() {
            state.error = true;
            state.status = "This input source has no usable physical key".to_string();
            return Vec::new();
        }
        let Some(action) = ACTIONS.get(target) else {
            state.capture = None;
            return Vec::new();
        };
        let action_id = action.id;
        swap_binding(state, target, binding);
        state.capture = None;
        state.status = format!("Saving {}…", action.title);
        state.error = false;
        return vec![set_keybinding(action_id, binding)];
    }

    match logical_key {
        "Escape" => {
            state.escape_pending = true;
            Vec::new()
        }
        "ArrowUp" => {
            move_selection(state, -1);
            Vec::new()
        }
        "ArrowDown" => {
            move_selection(state, 1);
            Vec::new()
        }
        "Enter" | "Return" | "NumpadEnter" => {
            state.capture = state.selected;
            state.status = "Press the new shortcut · Esc cancels".to_string();
            state.error = false;
            Vec::new()
        }
        "Backspace" => {
            state.query.pop();
            state.scroll = 0;
            state.ensure_selection();
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn move_selection(state: &mut KeybindingsState, delta: isize) {
    let filtered = state.filtered();
    if filtered.is_empty() {
        state.selected = None;
        return;
    }
    let current = state
        .selected
        .and_then(|selected| filtered.iter().position(|candidate| *candidate == selected))
        .unwrap_or_default();
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(filtered.len() - 1)
    };
    state.selected = filtered.get(next).copied();
    state.ensure_selected_visible();
}

fn target_at(state: &KeybindingsState, x: f32, y: f32) -> Option<usize> {
    let window = state.window();
    if !window.contains(x, y) {
        return None;
    }
    let local_y = y - window.y;
    if local_y < HEADER_HEIGHT || local_y >= window.height - FOOTER_HEIGHT {
        return None;
    }
    let row = ((local_y - HEADER_HEIGHT) / ROW_HEIGHT)
        .floor()
        .max(0.0) as usize;
    state.filtered().get(state.scroll + row).copied()
}

fn swap_binding(state: &mut KeybindingsState, target: usize, binding: Binding) {
    let Some(action) = ACTIONS.get(target) else {
        return;
    };
    let previous = state.binding(action);
    let conflict = ACTIONS
        .iter()
        .enumerate()
        .find(|(index, candidate)| *index != target && state.binding(candidate) == binding)
        .map(|(index, _)| index);
    state.bindings.insert(action.id.to_string(), binding);
    if let Some(conflict) = conflict
        && let Some(conflicting_action) = ACTIONS.get(conflict)
    {
        state
            .bindings
            .insert(conflicting_action.id.to_string(), previous);
    }
}

fn hide(state: &mut KeybindingsState) -> Vec<ActorCommand> {
    state.visible = false;
    state.capture = None;
    state.hovered = None;
    state.pressed = None;
    state.backdrop_pressed = false;
    state.escape_pending = false;
    state.status.clear();
    redraw_or_record_error(state);
    (state.pane != 0)
        .then(|| layout_visibility(state.pane, false))
        .into_iter()
        .collect()
}

fn default_bindings() -> BTreeMap<String, Binding> {
    ACTIONS
        .iter()
        .map(|action| (action.id.to_string(), action.default_binding))
        .collect()
}

fn set_keybinding(action: &str, binding: Binding) -> ActorCommand {
    ActorCommand::SendHostMessage(
        serde_json::to_vec(&serde_json::json!({
            "type": "set-keybinding",
            "action": action,
            "key_code": binding.key_code,
            "modifiers": binding.modifiers,
        }))
        .unwrap_or_default(),
    )
}

fn layout_visibility(pane: u64, visible: bool) -> ActorCommand {
    send_local(
        LAYOUT_ACTOR_ID,
        serde_json::to_vec(&LayoutVisibility {
            message_type: "keybindings-pane",
            pane,
            visible,
        })
        .unwrap_or_default(),
    )
}

fn is_modifier_key(logical_key: &str) -> bool {
    matches!(
        logical_key,
        "Control" | "Shift" | "Alt" | "AltGraph" | "Meta" | "Super" | "Hyper"
    )
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    let query = query
        .trim()
        .to_lowercase()
        .chars()
        .filter(|character| !character.is_whitespace() && !matches!(character, '+' | '-' | '.'))
        .collect::<String>();
    if query.is_empty() {
        return Some(0);
    }
    let candidate = candidate
        .to_lowercase()
        .chars()
        .filter(|character| !character.is_whitespace() && !matches!(character, '+' | '-' | '.'))
        .collect::<String>();
    let mut query_chars = query.chars();
    let mut wanted = query_chars.next()?;
    let mut score = 0_i64;
    let mut previous = None;
    for (position, character) in candidate.chars().enumerate() {
        if character != wanted {
            continue;
        }
        score += 20;
        if position == 0 || previous.is_some_and(|previous: char| !previous.is_alphanumeric()) {
            score += 16;
        }
        match query_chars.next() {
            Some(next) => wanted = next,
            None => return Some(score - i64::try_from(position).unwrap_or(i64::MAX / 4).min(96)),
        }
        previous = Some(character);
    }
    None
}

fn binding_label(binding: Binding) -> String {
    let mut parts = Vec::new();
    if binding.modifiers & MOD_CTRL != 0 {
        parts.push("Ctrl".to_string());
    }
    if binding.modifiers & MOD_ALT != 0 {
        parts.push("Alt".to_string());
    }
    if binding.modifiers & MOD_SHIFT != 0 {
        parts.push("Shift".to_string());
    }
    if binding.modifiers & MOD_SUPER != 0 {
        parts.push("Super".to_string());
    }
    let key = brood_keybindings::key_name(binding.key_code);
    if key == "Key" {
        parts.push(format!("Key {}", binding.key_code));
    } else {
        parts.push(key.to_string());
    }
    parts.join("+")
}

fn redraw_or_record_error(state: &mut KeybindingsState) {
    redraw_for_resize_or_record_error(state, None);
}

fn redraw_for_resize_or_record_error(state: &mut KeybindingsState, resize_epoch: Option<u64>) {
    if state.pane == 0 {
        return;
    }
    if let Err(error) = redraw(state, resize_epoch) {
        state.error = true;
        state.status = error;
    }
}

fn redraw(state: &mut KeybindingsState, resize_epoch: Option<u64>) -> Result<(), String> {
    let window = state.window();
    let width = window.width;
    let height = window.height;
    let header = header_geometry(width);
    let row_layout = row_geometry(width);
    let mut packet = Packet::new();
    let mut resources = Vec::new();
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, state.height),
        color: SCRIM,
    });
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(
            window.x - 12.0,
            window.y - 6.0,
            window.width + 24.0,
            window.height + 28.0,
        ),
        radius: RADIUS + 12.0,
        color: SHADOW_WIDE,
    });
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(
            window.x - 5.0,
            window.y - 2.0,
            window.width + 10.0,
            window.height + 13.0,
        ),
        radius: RADIUS + 5.0,
        color: SHADOW_NEAR,
    });
    packet.push(Command::Save);
    packet.push(Command::SetTransform(Transform {
        tx: window.x,
        ty: window.y,
        ..Transform::IDENTITY
    }));
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(0.0, 0.0, width, height),
        radius: RADIUS,
        color: BG,
    });
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(0.0, 0.0, width, HEADER_HEIGHT),
        radius: RADIUS,
        color: PANEL,
    });
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, RADIUS, width, HEADER_HEIGHT - RADIUS),
        color: PANEL,
    });
    add_text(
        state,
        &mut packet,
        &mut resources,
        if header.title_width < 150.0 {
            "Shortcuts"
        } else {
            "Keyboard shortcuts"
        },
        18.0,
        TEXT,
        SIDE_MARGIN,
        11.0,
        header.title_width,
    )?;
    add_text(
        state,
        &mut packet,
        &mut resources,
        "Global key policy",
        10.0,
        MUTED,
        SIDE_MARGIN,
        36.0,
        header.title_width,
    )?;
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(header.search_x, 14.0, header.search_width, 36.0),
        radius: 5.0,
        color: PANEL_RAISED,
    });
    packet.push(Command::StrokeRoundedRect {
        rect: Rect::new(header.search_x, 14.0, header.search_width, 36.0),
        radius: 5.0,
        color: ACCENT,
        width: 1.0,
    });
    let query = if let Some(capture) = state.capture {
        ACTIONS.get(capture).map_or_else(
            || "Press a shortcut…".to_string(),
            |action| format!("Press a shortcut for {}…", action.title),
        )
    } else if state.query.is_empty() {
        "Fuzzy search actions…".to_string()
    } else {
        format!("{}▏", state.query)
    };
    add_text(
        state,
        &mut packet,
        &mut resources,
        &truncate_to_width(&query, header.search_width - 18.0, 13.0),
        13.0,
        if state.query.is_empty() && state.capture.is_none() {
            MUTED
        } else {
            FG
        },
        header.search_x + 9.0,
        23.0,
        header.search_width - 18.0,
    )?;

    let filtered = state.filtered();
    packet.push(Command::Save);
    packet.push(Command::ClipRect(Rect::new(
        0.0,
        HEADER_HEIGHT,
        width,
        height - HEADER_HEIGHT - FOOTER_HEIGHT,
    )));
    for (row, index) in filtered
        .iter()
        .skip(state.scroll)
        .take(state.visible_rows())
        .copied()
        .enumerate()
    {
        let Some(action) = ACTIONS.get(index) else {
            continue;
        };
        let y = HEADER_HEIGHT + row as f32 * ROW_HEIGHT;
        let selected = state.selected == Some(index);
        let hovered = state.hovered == Some(index);
        let pressed = state.pressed == Some(index);
        if selected || hovered || pressed {
            packet.push(Command::FillRect {
                rect: Rect::new(0.0, y, width, ROW_HEIGHT),
                color: if pressed {
                    ashen::INTERACTIVE_PRESSED
                } else if hovered {
                    ashen::INTERACTIVE_HOVER
                } else {
                    ashen::SELECTION
                },
            });
        }
        if state.capture == Some(index) {
            packet.push(Command::FillRect {
                rect: Rect::new(0.0, y, 4.0, ROW_HEIGHT),
                color: ACCENT,
            });
        }
        packet.push(Command::FillRect {
            rect: Rect::new(0.0, y + ROW_HEIGHT - 1.0, width, 1.0),
            color: Color::rgba(BORDER.r, BORDER.g, BORDER.b, 0.32),
        });
        add_text(
            state,
            &mut packet,
            &mut resources,
            &truncate_to_width(action.title, row_layout.title_width, 13.0),
            13.0,
            FG,
            SIDE_MARGIN,
            y + 4.0,
            row_layout.title_width,
        )?;
        let detail = format!(
            "{} · {} · {}",
            action.category,
            action.scope.label(),
            action.id
        );
        add_text(
            state,
            &mut packet,
            &mut resources,
            &truncate_to_width(&detail, row_layout.title_width, 9.0),
            9.0,
            MUTED,
            SIDE_MARGIN,
            y + 23.0,
            row_layout.title_width,
        )?;
        let binding = binding_label(state.binding(action));
        let binding_text_size = if row_layout.chip_width < 130.0 {
            9.5
        } else {
            11.0
        };
        packet.push(Command::FillRoundedRect {
            rect: Rect::new(row_layout.chip_x, y + 7.0, row_layout.chip_width, 28.0),
            radius: 5.0,
            color: PANEL_RAISED,
        });
        packet.push(Command::StrokeRoundedRect {
            rect: Rect::new(row_layout.chip_x, y + 7.0, row_layout.chip_width, 28.0),
            radius: 5.0,
            color: if state.capture == Some(index) {
                ACCENT
            } else {
                BORDER
            },
            width: 1.0,
        });
        add_text(
            state,
            &mut packet,
            &mut resources,
            &truncate_to_width(&binding, row_layout.chip_width - 16.0, binding_text_size),
            binding_text_size,
            TEXT,
            row_layout.chip_x + 8.0,
            y + 14.0,
            row_layout.chip_width - 16.0,
        )?;
    }
    packet.push(Command::Restore);

    if filtered.is_empty() {
        add_text(
            state,
            &mut packet,
            &mut resources,
            "No matching shortcuts",
            13.0,
            MUTED,
            SIDE_MARGIN,
            HEADER_HEIGHT + 14.0,
            width - SIDE_MARGIN * 2.0,
        )?;
    }

    let footer_y = height - FOOTER_HEIGHT;
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(0.0, footer_y, width, FOOTER_HEIGHT),
        radius: RADIUS,
        color: PANEL,
    });
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, footer_y, width, RADIUS),
        color: PANEL,
    });
    let footer = if state.status.is_empty() {
        format!(
            "{} shortcuts · ↑↓ select · Enter rebind · Esc close",
            filtered.len()
        )
    } else {
        state.status.clone()
    };
    add_text(
        state,
        &mut packet,
        &mut resources,
        &truncate_to_width(&footer, width - SIDE_MARGIN * 2.0, 10.0),
        10.0,
        if state.error { ERROR } else { MUTED },
        SIDE_MARGIN,
        footer_y + 9.0,
        width - SIDE_MARGIN * 2.0,
    )?;
    packet.push(Command::StrokeRoundedRect {
        rect: Rect::new(0.5, 0.5, width - 1.0, height - 1.0),
        radius: RADIUS,
        color: BORDER,
        width: 1.0,
    });
    packet.push(Command::Restore);

    submit_packet_for_resize(state.pane, resize_epoch, &packet)?;
    drop_resources(state.pane, state.text_resources.drain(..));
    state.text_resources = resources;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct HeaderGeometry {
    title_width: f32,
    search_x: f32,
    search_width: f32,
}

fn header_geometry(width: f32) -> HeaderGeometry {
    let desired_search_width = if width < 560.0 {
        (width * 0.42).max(120.0)
    } else {
        width - 250.0 - SIDE_MARGIN
    };
    let max_search_width = (width - SIDE_MARGIN * 3.0 - 80.0).max(1.0);
    let search_width = desired_search_width.min(max_search_width);
    let search_x = width - SIDE_MARGIN - search_width;
    HeaderGeometry {
        title_width: (search_x - SIDE_MARGIN * 2.0).max(1.0),
        search_x,
        search_width,
    }
}

#[derive(Clone, Copy, Debug)]
struct RowGeometry {
    title_width: f32,
    chip_x: f32,
    chip_width: f32,
}

fn row_geometry(width: f32) -> RowGeometry {
    let max_chip_width = (width - SIDE_MARGIN * 3.0 - 80.0).max(1.0);
    let chip_width = (width * 0.38)
        .clamp(96.0, MAX_BINDING_WIDTH)
        .min(max_chip_width);
    let chip_x = width - SIDE_MARGIN - chip_width;
    RowGeometry {
        title_width: (chip_x - SIDE_MARGIN * 2.0).max(1.0),
        chip_x,
        chip_width,
    }
}

#[allow(clippy::too_many_arguments)]
fn add_text(
    state: &KeybindingsState,
    packet: &mut Packet,
    resources: &mut Vec<u64>,
    text: &str,
    size: f32,
    color: Color,
    x: f32,
    y: f32,
    max_width: f32,
) -> Result<(), String> {
    let layout = layout_ui_text(state.pane, text, size, max_width, size + 4.0, color)
        .map_err(|error| format!("layout keybinding text: {error}"))?;
    resources.push(layout.handle);
    packet.push(Command::Save);
    packet.push(Command::ClipRect(Rect::new(
        x,
        y,
        max_width.max(1.0),
        size + 4.0,
    )));
    packet.push(Command::DrawTextLayout {
        layout: layout.handle,
        x,
        y,
    });
    packet.push(Command::Restore);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_search_matches_title_category_and_binding() {
        let mut state = KeybindingsState::new();
        state.query = "sess pick".to_string();
        assert!(state.filtered().contains(&8) || state.filtered().contains(&9));
        state.query = "ctrl shift /".to_string();
        assert_eq!(state.filtered().first().copied(), Some(0));
    }

    #[test]
    fn conflicting_remap_swaps_instead_of_shadowing() {
        let mut state = KeybindingsState::new();
        let first = state.binding(&ACTIONS[0]);
        let second = state.binding(&ACTIONS[1]);
        swap_binding(&mut state, 0, second);
        assert_eq!(state.binding(&ACTIONS[0]), second);
        assert_eq!(state.binding(&ACTIONS[1]), first);
    }

    #[test]
    fn floating_window_is_centered_and_uses_the_real_narrow_extent() {
        let mut state = KeybindingsState::new();
        state.resize(330.0, 300.0);
        assert_eq!((state.width, state.height), (330.0, 300.0));
        assert_eq!(state.visible_rows(), 4);

        let window = state.window();
        assert_eq!(window, FloatingWindow::in_viewport(330.0, 300.0));
        assert_eq!((window.x, window.y), (16.0, 16.0));
        assert_eq!((window.width, window.height), (298.0, 268.0));
        assert!(window.contains(16.0, 16.0));
        assert!(!window.contains(0.0, 0.0));

        let header = header_geometry(window.width);
        assert!(header.title_width > 0.0);
        assert!(header.search_width > 0.0);
        assert!(header.search_x + header.search_width <= window.width - SIDE_MARGIN);

        let row = row_geometry(window.width);
        assert!(row.title_width > 0.0);
        assert!(row.chip_width > 0.0);
        assert!(row.chip_x + row.chip_width <= window.width - SIDE_MARGIN);

        assert_eq!(target_at(&state, window.x + 1.0, window.y + HEADER_HEIGHT), Some(0));
        assert_eq!(target_at(&state, 0.0, 0.0), None);
    }
}

bindings::export!(KeybindingsActor with_types_in bindings);
