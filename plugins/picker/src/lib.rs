use std::{collections::VecDeque, sync::Mutex};

use brood_graphics::{Color, Command, Packet, Rect, theme::ashen};
use brood_plugin_sdk::{
    actor::send_local,
    bindings,
    exports::brood::plugin::actor::{
        Command as ActorCommand, Event, Guest, LifecycleEvent, PresentationEvent, WorkerEvent,
        WorkerRequest,
    },
    graphics::{drop_resources, layout_ui_text, submit_packet_for_resize},
    input::BUTTON_PRIMARY as LEFT_BUTTON,
    runtime::sleep,
    text::{truncate, truncate_to_width},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const LAYOUT_ACTOR_ID: &str = "session-switcher";
const HEADER_HEIGHT: f32 = 52.0;
const FOOTER_HEIGHT: f32 = 28.0;
const ROW_HEIGHT: f32 = 30.0;
const SIDE_MARGIN: f32 = 14.0;
const CONTAINER_RADIUS: f32 = 8.0;
const QUERY_X: f32 = 150.0;
const SPINNER_INTERVAL_MS: u64 = 90;

const BG: Color = ashen::BACKGROUND;
const PANEL: Color = ashen::SURFACE;
const PANEL_RAISED: Color = ashen::SURFACE_RAISED;
const HOVER: Color = ashen::INTERACTIVE_HOVER;
const PRESSED: Color = ashen::INTERACTIVE_PRESSED;
const PANEL_LIGHT: Color = ashen::BORDER;
const FG: Color = ashen::TEXT;
const TEXT: Color = ashen::TEXT_STRONG;
const MUTED: Color = ashen::MUTED;
const YELLOW: Color = ashen::ACCENT;
const RED: Color = ashen::ERROR;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum PickerMessage {
    #[serde(rename = "picker-create")]
    Create {
        #[serde(default)]
        request: Option<Value>,
        #[serde(default)]
        title: String,
        #[serde(default)]
        placeholder: String,
        #[serde(default)]
        items: Vec<Value>,
        #[serde(default)]
        loading: bool,
        #[serde(default)]
        loading_label: String,
    },
    #[serde(rename = "picker-add")]
    Add {
        picker: u64,
        #[serde(default)]
        items: Vec<Value>,
    },
    #[serde(rename = "picker-update")]
    Update {
        picker: u64,
        #[serde(default)]
        items: Vec<Value>,
        #[serde(default)]
        loading: bool,
        #[serde(default)]
        loading_label: String,
    },
    #[serde(rename = "picker-cancel")]
    Cancel { picker: u64 },
    #[serde(rename = "picker-close")]
    Close,
}

#[derive(Clone, Debug)]
struct PickerEntry {
    index: usize,
    label: String,
    detail: Option<String>,
    result: Value,
}

impl PickerEntry {
    fn from_value(index: usize, result: Value) -> Result<Self, String> {
        let (label, detail) = match &result {
            Value::String(label) => (label.clone(), None),
            Value::Object(object) => {
                let label = object
                    .get("label")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "picker object items require a string `label` field".to_string()
                    })?
                    .to_string();
                let detail = object
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                (label, detail)
            }
            _ => {
                return Err(
                    "picker items must be strings or objects with a string `label` field"
                        .to_string(),
                );
            }
        };
        Ok(Self {
            index,
            label,
            detail,
            result,
        })
    }
}

#[derive(Debug)]
struct PickerRequest {
    id: u64,
    owner: String,
    title: String,
    placeholder: String,
    items: Vec<PickerEntry>,
    query: String,
    selected: Option<usize>,
    scroll: usize,
    loading: bool,
    loading_label: String,
}

impl PickerRequest {
    fn new(
        id: u64,
        owner: String,
        title: String,
        placeholder: String,
        items: Vec<Value>,
        loading: bool,
        loading_label: String,
    ) -> Result<Self, String> {
        let entries = parse_entries(0, items)?;
        let selected = entries.first().map(|entry| entry.index);
        Ok(Self {
            id,
            owner,
            title,
            placeholder,
            items: entries,
            query: String::new(),
            selected,
            scroll: 0,
            loading,
            loading_label,
        })
    }

    fn add(&mut self, items: Vec<Value>) -> Result<(), String> {
        let start = self.items.len();
        self.items.extend(parse_entries(start, items)?);
        self.ensure_selection();
        Ok(())
    }

    fn update(
        &mut self,
        items: Vec<Value>,
        loading: bool,
        loading_label: String,
    ) -> Result<(), String> {
        self.items = parse_entries(0, items)?;
        self.loading = loading;
        self.loading_label = loading_label;
        self.scroll = 0;
        self.ensure_selection();
        Ok(())
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let mut matched = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(position, item)| {
                fuzzy_score(&self.query, &item.label).map(|score| (position, score))
            })
            .collect::<Vec<_>>();
        matched.sort_by(
            |(left_position, left_score), (right_position, right_score)| {
                right_score.cmp(left_score).then_with(|| {
                    self.items[*left_position]
                        .index
                        .cmp(&self.items[*right_position].index)
                })
            },
        );
        matched
            .into_iter()
            .map(|(position, _score)| position)
            .collect()
    }

    fn ensure_selection(&mut self) {
        let filtered = self.filtered_indices();
        if self.selected.is_some_and(|selected| {
            filtered
                .iter()
                .any(|position| self.items[*position].index == selected)
        }) {
            self.ensure_selected_visible();
            return;
        }
        self.selected = filtered.first().map(|position| self.items[*position].index);
        self.scroll = 0;
    }

    fn ensure_selected_visible(&mut self) {
        let filtered = self.filtered_indices();
        let Some(position) = self.selected.and_then(|selected| {
            filtered
                .iter()
                .position(|item| self.items[*item].index == selected)
        }) else {
            self.scroll = self.scroll.min(filtered.len().saturating_sub(1));
            return;
        };
        if position < self.scroll {
            self.scroll = position;
        }
    }
}

struct PickerState {
    pane: u64,
    width: f32,
    height: f32,
    next_id: u64,
    active: Option<PickerRequest>,
    queued: VecDeque<PickerRequest>,
    text_resources: Vec<u64>,
    status: String,
    error: bool,
    hovered: Option<usize>,
    pressed: Option<usize>,
    spinner_phase: usize,
    spinner_worker: Option<u64>,
    next_spinner_worker: u64,
}

impl PickerState {
    const fn new() -> Self {
        Self {
            pane: 0,
            width: 720.0,
            height: 480.0,
            next_id: 1,
            active: None,
            queued: VecDeque::new(),
            text_resources: Vec::new(),
            status: String::new(),
            error: false,
            hovered: None,
            pressed: None,
            spinner_phase: 0,
            spinner_worker: None,
            next_spinner_worker: 1,
        }
    }

    fn allocate_id(&mut self) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "picker ID space is exhausted".to_string())?;
        Ok(id)
    }

    fn visible_rows(&self) -> usize {
        ((self.height - HEADER_HEIGHT - FOOTER_HEIGHT) / ROW_HEIGHT)
            .floor()
            .max(1.0) as usize
    }

    fn find_mut(&mut self, id: u64) -> Option<&mut PickerRequest> {
        if self.active.as_ref().is_some_and(|picker| picker.id == id) {
            return self.active.as_mut();
        }
        self.queued.iter_mut().find(|picker| picker.id == id)
    }

    fn promote(&mut self) -> bool {
        self.active = self.queued.pop_front();
        self.status.clear();
        self.error = false;
        self.hovered = None;
        self.pressed = None;
        self.spinner_phase = 0;
        self.active.is_some()
    }

    fn start_spinner(&mut self) -> Option<ActorCommand> {
        if self.spinner_worker.is_some()
            || !self.active.as_ref().is_some_and(|picker| picker.loading)
        {
            return None;
        }
        let worker = self.next_spinner_worker;
        self.next_spinner_worker = self.next_spinner_worker.checked_add(1)?;
        self.spinner_worker = Some(worker);
        Some(ActorCommand::SpawnWorker(WorkerRequest {
            worker,
            payload: Vec::new(),
        }))
    }
}

static STATE: Mutex<PickerState> = Mutex::new(PickerState::new());

struct PickerActor;

impl Guest for PickerActor {
    fn dispatch(event: Event) -> Vec<ActorCommand> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Mailbox(message) => handle_message(&message.sender, &message.body),
            Event::Presentation(event) => handle_presentation(event),
            Event::Worker(event) => handle_worker(event),
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        sleep(SPINNER_INTERVAL_MS).await?;
        Ok(Vec::new())
    }
}

fn handle_message(sender: &str, bytes: &[u8]) -> Vec<ActorCommand> {
    let message = match serde_json::from_slice::<PickerMessage>(bytes) {
        Ok(message) => message,
        Err(error) => {
            return vec![reply(
                sender,
                &ErrorReply {
                    message_type: "picker-error",
                    picker: None,
                    message: format!("invalid picker message: {error}"),
                },
            )];
        }
    };
    match message {
        PickerMessage::Create {
            request,
            title,
            placeholder,
            items,
            loading,
            loading_label,
        } => create_picker(
            sender,
            request,
            title,
            placeholder,
            items,
            loading,
            loading_label,
        ),
        PickerMessage::Add { picker, items } => add_items(sender, picker, items),
        PickerMessage::Update {
            picker,
            items,
            loading,
            loading_label,
        } => update_picker(sender, picker, items, loading, loading_label),
        PickerMessage::Cancel { picker } => cancel_picker(sender, picker),
        PickerMessage::Close if sender == "pane-close" => {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            finish_active(&mut state, false)
        }
        PickerMessage::Close => Vec::new(),
    }
}

fn create_picker(
    sender: &str,
    correlation: Option<Value>,
    title: String,
    placeholder: String,
    items: Vec<Value>,
    loading: bool,
    loading_label: String,
) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let id = match state.allocate_id() {
        Ok(id) => id,
        Err(message) => {
            return vec![reply(
                sender,
                &ErrorReply {
                    message_type: "picker-error",
                    picker: None,
                    message,
                },
            )];
        }
    };
    let picker = match PickerRequest::new(
        id,
        sender.to_string(),
        title,
        placeholder,
        items,
        loading,
        loading_label,
    ) {
        Ok(picker) => picker,
        Err(message) => {
            return vec![reply(
                sender,
                &ErrorReply {
                    message_type: "picker-error",
                    picker: Some(id),
                    message,
                },
            )];
        }
    };
    let queued = state.active.is_some();
    if queued {
        state.queued.push_back(picker);
    } else {
        state.active = Some(picker);
        state.status.clear();
        state.error = false;
        state.hovered = None;
        state.pressed = None;
        redraw_or_record_error(&mut state);
    }
    let mut commands = vec![reply(
        sender,
        &CreatedReply {
            message_type: "picker-created",
            request: correlation,
            picker: id,
            queued,
        },
    )];
    if !queued && state.pane != 0 {
        commands.push(layout_visibility(state.pane, true));
    }
    if !queued && let Some(spinner) = state.start_spinner() {
        commands.push(spinner);
    }
    commands
}

fn add_items(sender: &str, picker: u64, items: Vec<Value>) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let active = state
        .active
        .as_ref()
        .is_some_and(|active| active.id == picker);
    let result = match state.find_mut(picker) {
        Some(request) if request.owner == sender => request.add(items),
        Some(_) => Err("only the actor that created a picker may update it".to_string()),
        None => Err("picker is unavailable".to_string()),
    };
    if let Err(message) = result {
        return vec![reply(
            sender,
            &ErrorReply {
                message_type: "picker-error",
                picker: Some(picker),
                message,
            },
        )];
    }
    if active {
        redraw_or_record_error(&mut state);
    }
    Vec::new()
}

fn update_picker(
    sender: &str,
    picker: u64,
    items: Vec<Value>,
    loading: bool,
    loading_label: String,
) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let active = state
        .active
        .as_ref()
        .is_some_and(|active| active.id == picker);
    let result = match state.find_mut(picker) {
        Some(request) if request.owner == sender => request.update(items, loading, loading_label),
        Some(_) => Err("only the actor that created a picker may update it".to_string()),
        None => Err("picker is unavailable".to_string()),
    };
    if let Err(message) = result {
        return vec![reply(
            sender,
            &ErrorReply {
                message_type: "picker-error",
                picker: Some(picker),
                message,
            },
        )];
    }
    if active {
        state.spinner_phase = 0;
        redraw_or_record_error(&mut state);
    }
    state.start_spinner().into_iter().collect()
}

fn handle_worker(event: WorkerEvent) -> Vec<ActorCommand> {
    let (worker, advance) = match event {
        WorkerEvent::Started(_) => return Vec::new(),
        WorkerEvent::Finished(finished) => (finished.worker, true),
        WorkerEvent::Failed(failed) => (failed.worker, false),
        WorkerEvent::Cancelled(worker) => (worker, false),
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if state.spinner_worker != Some(worker) {
        return Vec::new();
    }
    state.spinner_worker = None;
    if advance && state.active.as_ref().is_some_and(|picker| picker.loading) {
        state.spinner_phase = state.spinner_phase.wrapping_add(1) % 8;
        redraw_or_record_error(&mut state);
        return state.start_spinner().into_iter().collect();
    }
    Vec::new()
}

fn cancel_picker(sender: &str, picker: u64) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if state
        .active
        .as_ref()
        .is_some_and(|active| active.id == picker)
    {
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.owner != sender)
        {
            return vec![picker_error(
                sender,
                Some(picker),
                "only the actor that created a picker may cancel it",
            )];
        }
        return finish_active(&mut state, false);
    }
    let Some(position) = state.queued.iter().position(|queued| queued.id == picker) else {
        return vec![picker_error(sender, Some(picker), "picker is unavailable")];
    };
    if state
        .queued
        .get(position)
        .is_some_and(|queued| queued.owner != sender)
    {
        return vec![picker_error(
            sender,
            Some(picker),
            "only the actor that created a picker may cancel it",
        )];
    }
    let Some(cancelled) = state.queued.remove(position) else {
        return Vec::new();
    };
    vec![reply(
        &cancelled.owner,
        &CancelledReply {
            message_type: "picker-cancelled",
            picker: cancelled.id,
        },
    )]
}

fn handle_presentation(event: PresentationEvent) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match event {
        PresentationEvent::PaneCreated(created) => {
            state.pane = created.pane;
            state.width = created.width.max(320.0);
            state.height = created.height.max(240.0);
            redraw_or_record_error(&mut state);
            vec![layout_visibility(state.pane, state.active.is_some())]
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            state.width = resized.width.max(320.0);
            state.height = resized.height.max(240.0);
            state.hovered = None;
            state.pressed = None;
            ensure_active_selection_visible(&mut state);
            redraw_for_resize_or_record_error(&mut state, resized.resize_epoch);
            Vec::new()
        }
        PresentationEvent::Key(key) if key.pane == state.pane && key.pressed => {
            let commands = handle_key(&mut state, &key.logical_key);
            redraw_or_record_error(&mut state);
            commands
        }
        PresentationEvent::Text(text) if text.pane == state.pane => {
            if let Some(active) = state.active.as_mut() {
                active.query.extend(
                    text.text
                        .chars()
                        .filter(|character| !character.is_control()),
                );
                active.scroll = 0;
                active.ensure_selection();
            }
            state.hovered = None;
            state.pressed = None;
            redraw_or_record_error(&mut state);
            Vec::new()
        }
        PresentationEvent::PointerMoved(moved) if moved.pane == state.pane => {
            let hovered = picker_target_at(&state, moved.y);
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
            let target = picker_target_at(&state, button.y);
            let commands = if button.pressed {
                state.hovered = target;
                state.pressed = target;
                if let (Some(target), Some(active)) = (target, state.active.as_mut()) {
                    active.selected = Some(target);
                }
                Vec::new()
            } else {
                let pressed = state.pressed.take();
                state.hovered = target;
                match (pressed, target) {
                    (Some(pressed), Some(released)) if pressed == released => {
                        select_picker_target(&mut state, pressed)
                    }
                    _ => Vec::new(),
                }
            };
            redraw_or_record_error(&mut state);
            commands
        }
        PresentationEvent::PointerScroll(scroll) if scroll.pane == state.pane => {
            let visible_rows = state.visible_rows();
            if let Some(active) = state.active.as_mut() {
                let max_scroll = active.filtered_indices().len().saturating_sub(visible_rows);
                if scroll.vertical < 0.0 {
                    active.scroll = (active.scroll + 1).min(max_scroll);
                } else if scroll.vertical > 0.0 {
                    active.scroll = active.scroll.saturating_sub(1);
                }
            }
            state.hovered = None;
            state.pressed = None;
            redraw_or_record_error(&mut state);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn handle_key(state: &mut PickerState, key: &str) -> Vec<ActorCommand> {
    match key {
        "ArrowUp" => {
            move_selection(state, -1);
            Vec::new()
        }
        "ArrowDown" => {
            move_selection(state, 1);
            Vec::new()
        }
        "Backspace" => {
            if let Some(active) = state.active.as_mut() {
                active.query.pop();
                active.scroll = 0;
                active.ensure_selection();
            }
            Vec::new()
        }
        "Enter" | "Return" | "NumpadEnter" => select_active(state),
        "Escape" => finish_active(state, false),
        _ => Vec::new(),
    }
}

fn picker_target_at(state: &PickerState, y: f32) -> Option<usize> {
    if !y.is_finite() {
        return None;
    }
    if y < HEADER_HEIGHT || y >= state.height - FOOTER_HEIGHT {
        return None;
    }
    let row = ((y - HEADER_HEIGHT) / ROW_HEIGHT).floor().max(0.0) as usize;
    let active = state.active.as_ref()?;
    let filtered = active.filtered_indices();
    let position = filtered.get(active.scroll + row).copied()?;
    Some(active.items[position].index)
}

fn select_picker_target(state: &mut PickerState, target: usize) -> Vec<ActorCommand> {
    let Some(active) = state.active.as_mut() else {
        return Vec::new();
    };
    if !active.items.iter().any(|item| item.index == target) {
        return Vec::new();
    }
    active.selected = Some(target);
    select_active(state)
}

fn move_selection(state: &mut PickerState, delta: isize) {
    let visible_rows = state.visible_rows();
    let Some(active) = state.active.as_mut() else {
        return;
    };
    let filtered = active.filtered_indices();
    if filtered.is_empty() {
        active.selected = None;
        return;
    }
    let current = active
        .selected
        .and_then(|selected| {
            filtered
                .iter()
                .position(|position| active.items[*position].index == selected)
        })
        .unwrap_or_default();
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(filtered.len() - 1)
    };
    active.selected = Some(active.items[filtered[next]].index);
    if next < active.scroll {
        active.scroll = next;
    } else if next >= active.scroll + visible_rows {
        active.scroll = next + 1 - visible_rows;
    }
}

fn select_active(state: &mut PickerState) -> Vec<ActorCommand> {
    let Some(active) = state.active.as_ref() else {
        return Vec::new();
    };
    let Some(selected) = active.selected else {
        return Vec::new();
    };
    let Some(entry) = active.items.iter().find(|entry| entry.index == selected) else {
        return Vec::new();
    };
    let selected_reply = reply(
        &active.owner,
        &SelectedReply {
            message_type: "picker-selected",
            picker: active.id,
            result: entry.result.clone(),
            index: entry.index,
        },
    );
    let mut commands = vec![selected_reply];
    commands.extend(finish_active(state, true));
    commands
}

fn finish_active(state: &mut PickerState, selected: bool) -> Vec<ActorCommand> {
    let Some(finished) = state.active.take() else {
        return Vec::new();
    };
    state.hovered = None;
    state.pressed = None;
    let mut commands = Vec::new();
    if !selected {
        commands.push(reply(
            &finished.owner,
            &CancelledReply {
                message_type: "picker-cancelled",
                picker: finished.id,
            },
        ));
    }
    if state.promote() {
        redraw_or_record_error(state);
        commands.extend(state.start_spinner());
    } else if state.pane != 0 {
        commands.push(layout_visibility(state.pane, false));
    }
    commands
}

fn ensure_active_selection_visible(state: &mut PickerState) {
    let visible_rows = state.visible_rows();
    let Some(active) = state.active.as_mut() else {
        return;
    };
    let filtered = active.filtered_indices();
    let Some(position) = active.selected.and_then(|selected| {
        filtered
            .iter()
            .position(|item| active.items[*item].index == selected)
    }) else {
        return;
    };
    if position < active.scroll {
        active.scroll = position;
    } else if position >= active.scroll + visible_rows {
        active.scroll = position + 1 - visible_rows;
    }
}

fn parse_entries(start: usize, items: Vec<Value>) -> Result<Vec<PickerEntry>, String> {
    items
        .into_iter()
        .enumerate()
        .map(|(offset, item)| PickerEntry::from_value(start + offset, item))
        .collect()
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    let query = query.trim();
    if query.is_empty() {
        return Some(0);
    }
    let query = query
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>();
    let candidate_lower = candidate
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<Vec<_>>();
    let mut query_position = 0;
    let mut last_match = None;
    let mut score = 0_i64;
    for (position, character) in candidate_lower.iter().enumerate() {
        if query.get(query_position) != Some(character) {
            continue;
        }
        score += 20;
        if position == 0
            || candidate_lower
                .get(position.saturating_sub(1))
                .is_some_and(|previous| !previous.is_alphanumeric())
        {
            score += 18;
        }
        if last_match.is_some_and(|previous| previous + 1 == position) {
            score += 24;
        }
        if query_position == 0 {
            score -= i64::try_from(position).unwrap_or(i64::MAX / 4).min(64);
        }
        last_match = Some(position);
        query_position += 1;
        if query_position == query.len() {
            score -= i64::try_from(candidate_lower.len().saturating_sub(query.len()))
                .unwrap_or(i64::MAX / 4)
                .min(128);
            return Some(score);
        }
    }
    None
}

#[derive(Serialize)]
struct CreatedReply {
    #[serde(rename = "type")]
    message_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<Value>,
    picker: u64,
    queued: bool,
}

#[derive(Serialize)]
struct SelectedReply {
    #[serde(rename = "type")]
    message_type: &'static str,
    picker: u64,
    result: Value,
    index: usize,
}

#[derive(Serialize)]
struct CancelledReply {
    #[serde(rename = "type")]
    message_type: &'static str,
    picker: u64,
}

#[derive(Serialize)]
struct ErrorReply {
    #[serde(rename = "type")]
    message_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    picker: Option<u64>,
    message: String,
}

#[derive(Serialize)]
struct LayoutVisibility {
    #[serde(rename = "type")]
    message_type: &'static str,
    pane: u64,
    visible: bool,
}

fn reply(actor: &str, message: &impl Serialize) -> ActorCommand {
    let body = serde_json::to_vec(message).unwrap_or_else(|error| {
        format!(r#"{{"type":"picker-error","message":"serialize reply: {error}"}}"#).into_bytes()
    });
    send_local(actor, body)
}

fn picker_error(actor: &str, picker: Option<u64>, message: &str) -> ActorCommand {
    reply(
        actor,
        &ErrorReply {
            message_type: "picker-error",
            picker,
            message: message.to_string(),
        },
    )
}

fn layout_visibility(pane: u64, visible: bool) -> ActorCommand {
    reply(
        LAYOUT_ACTOR_ID,
        &LayoutVisibility {
            message_type: "picker-pane",
            pane,
            visible,
        },
    )
}

fn redraw_or_record_error(state: &mut PickerState) {
    redraw_for_resize_or_record_error(state, None);
}

fn redraw_for_resize_or_record_error(state: &mut PickerState, resize_epoch: Option<u64>) {
    if state.pane == 0 {
        return;
    }
    if let Err(error) = redraw(state, resize_epoch) {
        state.status = error;
        state.error = true;
    }
}

fn redraw(state: &mut PickerState, resize_epoch: Option<u64>) -> Result<(), String> {
    let mut packet = Packet::new();
    let mut resources = Vec::new();
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(0.0, 0.0, state.width, state.height),
        radius: CONTAINER_RADIUS,
        color: BG,
    });
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(0.0, 0.0, state.width, HEADER_HEIGHT),
        radius: CONTAINER_RADIUS,
        color: PANEL,
    });
    packet.push(Command::FillRect {
        rect: Rect::new(
            0.0,
            CONTAINER_RADIUS,
            state.width,
            HEADER_HEIGHT - CONTAINER_RADIUS,
        ),
        color: PANEL,
    });
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, HEADER_HEIGHT - 1.0, state.width, 1.0),
        color: PANEL_LIGHT,
    });

    if let Some(active) = state.active.as_ref() {
        let title = if active.title.trim().is_empty() {
            "Select an item"
        } else {
            &active.title
        };
        let query_x = if state.width < 360.0 {
            (state.width * 0.38).clamp(44.0, QUERY_X)
        } else {
            QUERY_X
        };
        let title_width = (query_x - SIDE_MARGIN * 2.0).max(1.0);
        add_text(
            state,
            &mut packet,
            &mut resources,
            &truncate_to_width(title, title_width, 17.0),
            17.0,
            TEXT,
            SIDE_MARGIN,
            14.0,
            title_width,
        )?;
        let query_width = (state.width - query_x - SIDE_MARGIN).max(1.0);
        packet.push(Command::FillRoundedRect {
            rect: Rect::new(query_x, 10.0, query_width, 32.0),
            radius: 4.0,
            color: PANEL_RAISED,
        });
        packet.push(Command::StrokeRoundedRect {
            rect: Rect::new(query_x, 10.0, query_width, 32.0),
            radius: 4.0,
            color: YELLOW,
            width: 1.0,
        });
        let query = if active.query.is_empty() {
            if active.placeholder.is_empty() {
                "Type to filter…".to_string()
            } else {
                active.placeholder.clone()
            }
        } else {
            format!("{}▏", active.query)
        };
        add_text(
            state,
            &mut packet,
            &mut resources,
            &truncate_to_width(&query, query_width - 16.0, 14.0),
            14.0,
            if active.query.is_empty() { MUTED } else { FG },
            query_x + 8.0,
            16.0,
            query_width - 16.0,
        )?;

        let filtered = active.filtered_indices();
        let visible_rows = state.visible_rows();
        packet.push(Command::Save);
        packet.push(Command::ClipRect(Rect::new(
            0.0,
            HEADER_HEIGHT,
            state.width,
            state.height - HEADER_HEIGHT - FOOTER_HEIGHT,
        )));
        for (row, position) in filtered
            .iter()
            .skip(active.scroll)
            .take(visible_rows)
            .copied()
            .enumerate()
        {
            let item = &active.items[position];
            let y = HEADER_HEIGHT + row as f32 * ROW_HEIGHT;
            let selected = active.selected == Some(item.index);
            let pressed = state.pressed == Some(item.index);
            let hovered = state.hovered == Some(item.index);
            if pressed || hovered || selected {
                packet.push(Command::FillRect {
                    rect: Rect::new(0.0, y, state.width, ROW_HEIGHT),
                    color: if pressed {
                        PRESSED
                    } else if hovered {
                        HOVER
                    } else {
                        ashen::SELECTION
                    },
                });
            }
            if selected {
                packet.push(Command::FillRect {
                    rect: Rect::new(0.0, y, 3.0, ROW_HEIGHT),
                    color: YELLOW,
                });
            }
            packet.push(Command::FillRect {
                rect: Rect::new(0.0, y + ROW_HEIGHT - 1.0, state.width, 1.0),
                color: Color::rgba(PANEL_LIGHT.r, PANEL_LIGHT.g, PANEL_LIGHT.b, 0.28),
            });
            add_text(
                state,
                &mut packet,
                &mut resources,
                &truncate_to_width(&item.label, state.width - SIDE_MARGIN * 2.0, 13.0),
                13.0,
                FG,
                SIDE_MARGIN,
                if item.detail.is_some() { y } else { y + 6.0 },
                state.width - SIDE_MARGIN * 2.0,
            )?;
            if let Some(detail) = item.detail.as_deref() {
                add_text(
                    state,
                    &mut packet,
                    &mut resources,
                    &truncate_to_width(detail, state.width - SIDE_MARGIN * 2.0, 10.0),
                    10.0,
                    MUTED,
                    SIDE_MARGIN,
                    y + 16.0,
                    state.width - SIDE_MARGIN * 2.0,
                )?;
            }
        }
        packet.push(Command::Restore);

        if active.loading {
            let loading_row = filtered
                .len()
                .saturating_sub(active.scroll)
                .min(visible_rows - 1);
            let loading_y = HEADER_HEIGHT + loading_row as f32 * ROW_HEIGHT;
            draw_spinner(&mut packet, state.spinner_phase, loading_y + 15.0);
            let loading_label = if active.loading_label.trim().is_empty() {
                "Loading…"
            } else {
                active.loading_label.as_str()
            };
            add_text(
                state,
                &mut packet,
                &mut resources,
                loading_label,
                13.0,
                MUTED,
                SIDE_MARGIN + 24.0,
                loading_y + 9.0,
                state.width - SIDE_MARGIN * 2.0 - 24.0,
            )?;
        } else if filtered.is_empty() {
            add_text(
                state,
                &mut packet,
                &mut resources,
                "No matching items",
                13.0,
                MUTED,
                SIDE_MARGIN,
                HEADER_HEIGHT + 9.0,
                state.width - SIDE_MARGIN * 2.0,
            )?;
        }

        let footer = if active.loading {
            if active.loading_label.trim().is_empty() {
                "Loading…".to_string()
            } else {
                active.loading_label.clone()
            }
        } else if !state.status.is_empty() {
            state.status.clone()
        } else if let Some(hovered) = state.hovered {
            let label = active
                .items
                .iter()
                .find(|item| item.index == hovered)
                .map_or("item", |item| item.label.as_str());
            format!("Click to select {}", truncate(label, 42))
        } else {
            format!(
                "{} matches  ·  ↑↓ move  ·  Enter select  ·  Esc cancel",
                filtered.len()
            )
        };
        let footer_y = state.height - FOOTER_HEIGHT;
        packet.push(Command::FillRoundedRect {
            rect: Rect::new(0.0, footer_y, state.width, FOOTER_HEIGHT),
            radius: CONTAINER_RADIUS,
            color: PANEL,
        });
        packet.push(Command::FillRect {
            rect: Rect::new(0.0, footer_y, state.width, CONTAINER_RADIUS + 1.0),
            color: PANEL,
        });
        add_text(
            state,
            &mut packet,
            &mut resources,
            &truncate_to_width(&footer, state.width - SIDE_MARGIN * 2.0, 10.0),
            10.0,
            if state.error { RED } else { MUTED },
            SIDE_MARGIN,
            state.height - 21.0,
            state.width - SIDE_MARGIN * 2.0,
        )?;
    }

    packet.push(Command::StrokeRoundedRect {
        rect: Rect::new(0.5, 0.5, state.width - 1.0, state.height - 1.0),
        radius: CONTAINER_RADIUS,
        color: PANEL_LIGHT,
        width: 1.0,
    });

    submit_packet_for_resize(state.pane, resize_epoch, &packet)?;
    drop_resources(state.pane, state.text_resources.drain(..));
    state.text_resources = resources;
    Ok(())
}

fn draw_spinner(packet: &mut Packet, phase: usize, center_y: f32) {
    let center_x = SIDE_MARGIN + 7.0;
    let points = [
        (0.0, -7.0),
        (5.0, -5.0),
        (7.0, 0.0),
        (5.0, 5.0),
        (0.0, 7.0),
        (-5.0, 5.0),
        (-7.0, 0.0),
        (-5.0, -5.0),
    ];
    for (index, (x, y)) in points.into_iter().enumerate() {
        let distance = (index + 8 - phase % 8) % 8;
        let alpha = 1.0 - distance as f32 * 0.1;
        packet.push(Command::FillRoundedRect {
            rect: Rect::new(center_x + x - 1.5, center_y + y - 1.5, 3.0, 3.0),
            radius: 1.5,
            color: Color::rgba(YELLOW.r, YELLOW.g, YELLOW.b, alpha.max(0.3)),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn add_text(
    state: &PickerState,
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
        .map_err(|error| format!("layout picker text: {error}"))?;
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
    use serde_json::json;

    fn request(id: u64, owner: &str, labels: &[&str]) -> PickerRequest {
        PickerRequest::new(
            id,
            owner.to_string(),
            String::new(),
            String::new(),
            labels.iter().map(|label| json!(label)).collect(),
            false,
            String::new(),
        )
        .expect("valid picker request")
    }

    #[test]
    fn fuzzy_match_is_subsequence_ranked_by_boundaries_and_contiguity() {
        assert!(fuzzy_score("ff", "Find files").is_some());
        assert!(fuzzy_score("ff", "Files").is_none());
        assert!(
            fuzzy_score("sw", "Session workspace").expect("boundary match")
                > fuzzy_score("sw", "slow window").expect("sparse match")
        );
    }

    #[test]
    fn streamed_items_keep_append_order_indices() {
        let mut picker = request(7, "caller", &["alpha"]);
        picker
            .add(vec![json!({"label":"beta","value":2}), json!("gamma")])
            .expect("append picker items");
        assert_eq!(
            picker
                .items
                .iter()
                .map(|item| item.index)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn queued_pickers_promote_in_fifo_order() {
        let mut state = PickerState::new();
        state.active = Some(request(1, "first", &["one"]));
        state.queued.push_back(request(2, "second", &["two"]));
        state.queued.push_back(request(3, "third", &["three"]));
        assert!(state.promote());
        assert_eq!(state.active.as_ref().map(|picker| picker.id), Some(2));
        assert!(state.promote());
        assert_eq!(state.active.as_ref().map(|picker| picker.id), Some(3));
    }

    #[test]
    fn filtering_preserves_original_item_index() {
        let mut picker = request(1, "caller", &["alpha", "beta", "alpine"]);
        picker.query = "bt".to_string();
        picker.ensure_selection();
        assert_eq!(picker.selected, Some(1));
    }

    #[test]
    fn selection_replies_with_original_result_and_index() {
        let mut state = PickerState::new();
        state.active = Some(
            PickerRequest::new(
                9,
                "caller".to_string(),
                "Choose".to_string(),
                String::new(),
                vec![json!("alpha"), json!({"label":"beta","value":{"id":42}})],
                false,
                String::new(),
            )
            .expect("valid picker request"),
        );
        if let Some(active) = state.active.as_mut() {
            active.selected = Some(1);
        }
        let commands = select_active(&mut state);
        let Some(ActorCommand::SendLocalMessage(message)) = commands.first() else {
            panic!("selection did not send an actor reply");
        };
        assert_eq!(message.actor, "caller");
        let body: Value = serde_json::from_slice(&message.body).expect("valid selection reply");
        assert_eq!(body["type"], "picker-selected");
        assert_eq!(body["picker"], 9);
        assert_eq!(body["index"], 1);
        assert_eq!(body["result"]["value"]["id"], 42);
    }

    #[test]
    fn update_replaces_loading_results_without_resetting_the_query() {
        let mut picker = PickerRequest::new(
            4,
            "caller".to_string(),
            "Backends".to_string(),
            String::new(),
            Vec::new(),
            true,
            "Discovering backends…".to_string(),
        )
        .expect("valid loading picker");
        picker.query = "loc".to_string();
        picker
            .update(
                vec![json!({"label":"Local","value":{"backend":1}})],
                false,
                String::new(),
            )
            .expect("replace picker results");
        assert!(!picker.loading);
        assert_eq!(picker.query, "loc");
        assert_eq!(picker.selected, Some(0));
    }
}

bindings::export!(PickerActor with_types_in bindings);
