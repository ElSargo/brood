use std::{collections::BTreeMap, sync::Mutex};

use brood_graphics::{Color, Command, Packet, Rect, theme::ashen};
use brood_keybindings::{ACTIONS, Action, Binding, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_SUPER};
use brood_plugin_sdk::{
    actor::send_local,
    bindings,
    exports::brood::plugin::actor::{
        Command as ActorCommand, Event, Guest, LifecycleEvent, PresentationEvent,
    },
    graphics::UiFrame,
    input::BUTTON_PRIMARY as LEFT_BUTTON,
    text::truncate_to_width,
};
use serde::{Deserialize, Serialize};

const LAYOUT_ACTOR_ID: &str = "session-switcher";
const PICKER_ACTOR_ID: &str = "picker";
const PANEL_WIDTH: f32 = 620.0;
const PANEL_HEIGHT: f32 = 190.0;
const PANEL_MARGIN: f32 = 16.0;
const SIDE_MARGIN: f32 = 20.0;
const RADIUS: f32 = 9.0;

const BG: Color = ashen::BACKGROUND;
const PANEL_RAISED: Color = ashen::SURFACE_RAISED;
const BORDER: Color = ashen::BORDER;
const TEXT: Color = ashen::TEXT_STRONG;
const FG: Color = ashen::TEXT;
const MUTED: Color = ashen::MUTED;
const ACCENT: Color = ashen::ACCENT;
const ERROR: Color = ashen::ERROR;
const SCRIM: Color = Color::rgba(0.0, 0.0, 0.0, 0.48);

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
    #[serde(rename = "picker-created")]
    PickerCreated { picker: u64 },
    #[serde(rename = "picker-selected")]
    PickerSelected {
        picker: u64,
        result: PickerResult,
    },
    #[serde(rename = "picker-cancelled")]
    PickerCancelled { picker: u64 },
    #[serde(rename = "picker-error")]
    PickerError {
        #[serde(default)]
        picker: Option<u64>,
        message: String,
    },
}

#[derive(Clone, Deserialize)]
struct BindingOverride {
    action: String,
    key_code: i32,
    modifiers: i32,
}

#[derive(Deserialize)]
struct PickerResult {
    value: PickerValue,
}

#[derive(Deserialize)]
struct PickerValue {
    action: String,
}

#[derive(Serialize)]
struct PickerRequest {
    #[serde(rename = "type")]
    message_type: &'static str,
    title: &'static str,
    placeholder: &'static str,
    items: Vec<PickerItem>,
}

#[derive(Serialize)]
struct PickerItem {
    label: &'static str,
    detail: String,
    value: PickerItemValue,
}

#[derive(Serialize)]
struct PickerItemValue {
    action: &'static str,
}

#[derive(Serialize)]
struct PickerCancel {
    #[serde(rename = "type")]
    message_type: &'static str,
    picker: u64,
}

#[derive(Serialize)]
struct LayoutVisibility {
    #[serde(rename = "type")]
    message_type: &'static str,
    pane: u64,
    visible: bool,
}

#[derive(Clone, Copy)]
struct FloatingPanel {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl FloatingPanel {
    fn in_viewport(width: f32, height: f32) -> Self {
        let width = width.max(1.0);
        let height = height.max(1.0);
        let panel_width = PANEL_WIDTH.min((width - PANEL_MARGIN * 2.0).max(1.0));
        let panel_height = PANEL_HEIGHT.min((height - PANEL_MARGIN * 2.0).max(1.0));
        Self {
            x: (width - panel_width) * 0.5,
            y: (height - panel_height) * 0.5,
            width: panel_width,
            height: panel_height,
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

struct KeybindingsState {
    pane: u64,
    width: f32,
    height: f32,
    visible: bool,
    picker: Option<u64>,
    picker_requested: bool,
    capture: Option<usize>,
    pending_save: Option<usize>,
    bindings: BTreeMap<String, Binding>,
    status: String,
    error: bool,
    backdrop_pressed: bool,
    text_resources: Vec<u64>,
}

impl KeybindingsState {
    fn new() -> Self {
        Self {
            pane: 0,
            width: PANEL_WIDTH + PANEL_MARGIN * 2.0,
            height: PANEL_HEIGHT + PANEL_MARGIN * 2.0,
            visible: false,
            picker: None,
            picker_requested: false,
            capture: None,
            pending_save: None,
            bindings: default_bindings(),
            status: String::new(),
            error: false,
            backdrop_pressed: false,
            text_resources: Vec::new(),
        }
    }

    fn panel(&self) -> FloatingPanel {
        FloatingPanel::in_viewport(self.width, self.height)
    }

    fn capture_action(&self) -> Option<&'static Action> {
        self.capture.and_then(|index| ACTIONS.get(index))
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
            Event::Mailbox(message) => handle_message(state, &message.sender, &message.body),
            Event::Presentation(event) => handle_presentation(state, event),
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

fn handle_message(
    state: &mut KeybindingsState,
    sender: &str,
    bytes: &[u8],
) -> Vec<ActorCommand> {
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
            if state.picker_requested || state.picker.is_some() || state.visible {
                return Vec::new();
            }
            open_action_picker(state).into_iter().collect()
        }
        KeybindingsMessage::Close => close_editor(state),
        KeybindingsMessage::Saved {
            action,
            success,
            message,
        } => handle_saved(state, &action, success, message),
        KeybindingsMessage::PickerCreated { picker } if sender == PICKER_ACTOR_ID => {
            state.picker_requested = false;
            state.picker = Some(picker);
            Vec::new()
        }
        KeybindingsMessage::PickerSelected { picker, result }
            if sender == PICKER_ACTOR_ID && state.picker == Some(picker) =>
        {
            state.picker = None;
            let Some(index) = ACTIONS
                .iter()
                .position(|action| action.id == result.value.action)
            else {
                state.error = true;
                state.status = "The selected shortcut is unavailable".to_string();
                return Vec::new();
            };
            state.capture = Some(index);
            state.visible = true;
            state.status = "Press the new shortcut · Esc cancels".to_string();
            state.error = false;
            redraw_or_record_error(state);
            (state.pane != 0)
                .then(|| layout_visibility(state.pane, true))
                .into_iter()
                .collect()
        }
        KeybindingsMessage::PickerCancelled { picker }
            if sender == PICKER_ACTOR_ID && state.picker == Some(picker) =>
        {
            state.picker = None;
            state.picker_requested = false;
            Vec::new()
        }
        KeybindingsMessage::PickerError { picker, message } if sender == PICKER_ACTOR_ID => {
            if picker.is_none() || picker == state.picker {
                state.picker = None;
                state.picker_requested = false;
                state.error = true;
                state.status = message;
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn open_action_picker(state: &mut KeybindingsState) -> Option<ActorCommand> {
    let items = ACTIONS
        .iter()
        .map(|action| PickerItem {
            label: action.title,
            detail: format!(
                "{} · {} · {} · {}",
                action.category,
                action.scope.label(),
                binding_label(state.binding(action)),
                action.id
            ),
            value: PickerItemValue { action: action.id },
        })
        .collect();
    let body = serde_json::to_vec(&PickerRequest {
        message_type: "picker-create",
        title: "Keyboard shortcuts",
        placeholder: "Fuzzy-match an action or binding…",
        items,
    })
    .ok()?;
    state.picker_requested = true;
    state.error = false;
    Some(send_local(PICKER_ACTOR_ID, body))
}

fn handle_saved(
    state: &mut KeybindingsState,
    action: &str,
    success: bool,
    message: String,
) -> Vec<ActorCommand> {
    let saved = state.pending_save.take().filter(|index| {
        ACTIONS
            .get(*index)
            .is_some_and(|candidate| candidate.id == action)
    });
    if !success {
        state.capture = saved;
        state.error = true;
        state.status = if message.is_empty() {
            "Could not save shortcut".to_string()
        } else {
            message
        };
        redraw_or_record_error(state);
        return Vec::new();
    }

    state.capture = None;
    state.visible = false;
    state.error = false;
    state.status.clear();
    redraw_or_record_error(state);
    let mut commands = Vec::new();
    if state.pane != 0 {
        commands.push(layout_visibility(state.pane, false));
    }
    commands.extend(open_action_picker(state));
    commands
}

fn handle_presentation(
    state: &mut KeybindingsState,
    event: PresentationEvent,
) -> Vec<ActorCommand> {
    match event {
        PresentationEvent::PaneCreated(created) => {
            state.pane = created.pane;
            state.width = created.width.max(1.0);
            state.height = created.height.max(1.0);
            redraw_or_record_error(state);
            vec![layout_visibility(state.pane, false)]
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            state.width = resized.width.max(1.0);
            state.height = resized.height.max(1.0);
            state.backdrop_pressed = false;
            redraw_for_resize_or_record_error(state, resized.resize_epoch);
            Vec::new()
        }
        PresentationEvent::Key(key) if key.pane == state.pane && !key.repeat => {
            if !key.pressed || state.capture.is_none() {
                return Vec::new();
            }
            let commands = capture_key(
                state,
                key.physical_key,
                &key.logical_key,
                i32::try_from(key.modifiers).unwrap_or(i32::MAX),
            );
            redraw_or_record_error(state);
            commands
        }
        PresentationEvent::PointerButton(button)
            if button.pane == state.pane && button.button == LEFT_BUTTON =>
        {
            let outside = !state.panel().contains(button.x, button.y);
            if button.pressed {
                state.backdrop_pressed = outside;
                Vec::new()
            } else if state.backdrop_pressed && outside {
                state.backdrop_pressed = false;
                close_editor(state)
            } else {
                state.backdrop_pressed = false;
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn capture_key(
    state: &mut KeybindingsState,
    physical_key: u32,
    logical_key: &str,
    modifiers: i32,
) -> Vec<ActorCommand> {
    if logical_key == "Escape" && modifiers == 0 {
        return close_editor(state);
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
    let Some(target) = state.capture else {
        return Vec::new();
    };
    let Some(action) = ACTIONS.get(target) else {
        return close_editor(state);
    };
    let action_id = action.id;
    let title = action.title;
    swap_binding(state, target, binding);
    state.capture = None;
    state.pending_save = Some(target);
    state.status = format!("Saving {title}…");
    state.error = false;
    vec![set_keybinding(action_id, binding)]
}

fn close_editor(state: &mut KeybindingsState) -> Vec<ActorCommand> {
    let mut commands = Vec::new();
    if let Some(picker) = state.picker.take()
        && let Ok(body) = serde_json::to_vec(&PickerCancel {
            message_type: "picker-cancel",
            picker,
        })
    {
        commands.push(send_local(PICKER_ACTOR_ID, body));
    }
    state.picker_requested = false;
    state.visible = false;
    state.capture = None;
    state.pending_save = None;
    state.backdrop_pressed = false;
    state.status.clear();
    redraw_or_record_error(state);
    if state.pane != 0 {
        commands.push(layout_visibility(state.pane, false));
    }
    commands
}

impl KeybindingsState {
    fn binding(&self, action: &Action) -> Binding {
        self.bindings
            .get(action.id)
            .copied()
            .unwrap_or(action.default_binding)
    }
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
    let panel = state.panel();
    let mut packet = Packet::new();
    let mut frame = UiFrame::new(state.pane);
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, state.height),
        color: SCRIM,
    });
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(panel.x, panel.y, panel.width, panel.height),
        radius: RADIUS,
        color: BG,
    });
    packet.push(Command::StrokeRoundedRect {
        rect: Rect::new(
            panel.x + 0.5,
            panel.y + 0.5,
            (panel.width - 1.0).max(1.0),
            (panel.height - 1.0).max(1.0),
        ),
        radius: RADIUS,
        color: BORDER,
        width: 1.0,
    });

    let action = state.capture_action();
    let title = action.map_or("Keyboard shortcut", |action| action.title);
    add_text(
        state,
        &mut packet,
        &mut frame,
        title,
        18.0,
        TEXT,
        panel.x + SIDE_MARGIN,
        panel.y + 20.0,
        panel.width - SIDE_MARGIN * 2.0,
    )?;
    let detail = action.map_or_else(
        || "Waiting for an action from the shared picker".to_string(),
        |action| {
            format!(
                "{} · {} · currently {}",
                action.category,
                action.scope.label(),
                binding_label(state.binding(action))
            )
        },
    );
    add_text(
        state,
        &mut packet,
        &mut frame,
        &truncate_to_width(&detail, panel.width - SIDE_MARGIN * 2.0, 12.0),
        12.0,
        MUTED,
        panel.x + SIDE_MARGIN,
        panel.y + 52.0,
        panel.width - SIDE_MARGIN * 2.0,
    )?;
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(
            panel.x + SIDE_MARGIN,
            panel.y + 86.0,
            panel.width - SIDE_MARGIN * 2.0,
            48.0,
        ),
        radius: 6.0,
        color: PANEL_RAISED,
    });
    packet.push(Command::StrokeRoundedRect {
        rect: Rect::new(
            panel.x + SIDE_MARGIN,
            panel.y + 86.0,
            panel.width - SIDE_MARGIN * 2.0,
            48.0,
        ),
        radius: 6.0,
        color: if state.error { ERROR } else { ACCENT },
        width: 1.0,
    });
    add_text(
        state,
        &mut packet,
        &mut frame,
        if state.status.is_empty() {
            "Press the new shortcut · Esc cancels"
        } else {
            &state.status
        },
        13.0,
        if state.error { ERROR } else { FG },
        panel.x + SIDE_MARGIN + 14.0,
        panel.y + 101.0,
        panel.width - SIDE_MARGIN * 2.0 - 28.0,
    )?;
    add_text(
        state,
        &mut packet,
        &mut frame,
        "Action selection is provided by the shared picker",
        10.0,
        MUTED,
        panel.x + SIDE_MARGIN,
        panel.y + panel.height - 28.0,
        panel.width - SIDE_MARGIN * 2.0,
    )?;

    frame.submit(resize_epoch, &packet, &mut state.text_resources)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_text(
    _state: &KeybindingsState,
    packet: &mut Packet,
    frame: &mut UiFrame,
    text: &str,
    size: f32,
    color: Color,
    x: f32,
    y: f32,
    max_width: f32,
) -> Result<(), String> {
    frame
        .draw_text_clipped(
            packet,
            text,
            size,
            max_width,
            size + 4.0,
            color,
            x,
            y,
        )
        .map_err(|error| format!("layout keybinding text: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_picker_contains_every_binding_and_preserves_action_identity() {
        let mut state = KeybindingsState::new();
        let ActorCommand::SendLocalMessage(message) =
            open_action_picker(&mut state).expect("picker request")
        else {
            panic!("action picker did not use a local actor message");
        };
        assert_eq!(message.actor, PICKER_ACTOR_ID);
        let value: serde_json::Value = serde_json::from_slice(&message.body).unwrap();
        assert_eq!(value["type"], "picker-create");
        assert_eq!(value["items"].as_array().map(Vec::len), Some(ACTIONS.len()));
        assert_eq!(value["items"][0]["value"]["action"], ACTIONS[0].id);
    }

    #[test]
    fn shared_picker_selection_enters_only_chord_capture() {
        let mut state = KeybindingsState::new();
        state.picker = Some(7);
        let commands = handle_message(
            &mut state,
            PICKER_ACTOR_ID,
            br#"{"type":"picker-selected","picker":7,"result":{"label":"Open session picker","value":{"action":"sessions.picker"}},"index":4}"#,
        );
        assert_eq!(state.capture, ACTIONS.iter().position(|action| action.id == "sessions.picker"));
        assert!(state.visible);
        assert!(commands.is_empty());
    }

    #[test]
    fn action_picker_uses_current_persisted_overrides_in_details() {
        let mut state = KeybindingsState::new();
        let override_binding = Binding::new(31, MOD_CTRL | MOD_ALT);
        state
            .bindings
            .insert("sessions.picker".to_string(), override_binding);
        let ActorCommand::SendLocalMessage(message) =
            open_action_picker(&mut state).expect("picker request")
        else {
            panic!("action picker did not use a local actor message");
        };
        let value: serde_json::Value = serde_json::from_slice(&message.body).unwrap();
        let item = value["items"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item["value"]["action"] == "sessions.picker")
            })
            .expect("session picker binding");
        assert!(
            item["detail"]
                .as_str()
                .unwrap()
                .contains(&binding_label(override_binding))
        );
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
    fn compact_capture_panel_stays_inside_narrow_viewport() {
        let panel = FloatingPanel::in_viewport(360.0, 320.0);
        assert!(panel.x >= 0.0 && panel.y >= 0.0);
        assert!(panel.x + panel.width <= 360.0);
        assert!(panel.y + panel.height <= 320.0);
    }
}

bindings::export!(KeybindingsActor with_types_in bindings);
