wit_bindgen::generate!({
    path: "../../crates/manifold-wasm/wit",
    world: "manifold-plugin",
});

use std::sync::Mutex;

use exports::manifold::plugin::actor::{
    ActorStartRequest, Command as ActorCommand, Event, Guest, LifecycleEvent, PresentationEvent,
    SessionCommand, SessionCommandCompleted, SessionEvent, SessionOperation, SessionSnapshot,
};
use manifold::plugin::host;
use manifold_graphics::{Color, Command, Packet, Rect};
use serde::Deserialize;

const INIT_ACTOR_ID: &str = "manifold.client-init";
const CHILD_ACTOR_ID: &str = "session-switcher";
const COMPONENT_FILE: &str = "manifold_session_switcher_plugin.component.wasm";
const LEFT_BUTTON: u32 = 0x110;
const RIGHT_BUTTON: u32 = 0x111;
const HEADER_HEIGHT: f32 = 104.0;
const FOOTER_HEIGHT: f32 = 38.0;
const ROW_HEIGHT: f32 = 46.0;
const SIDE_MARGIN: f32 = 14.0;
const SIDEBAR_WIDTH: f64 = 320.0;

const BG: Color = Color::rgba(0.157, 0.157, 0.157, 1.0);
const PANEL: Color = Color::rgba(0.235, 0.220, 0.204, 1.0);
const PANEL_DARK: Color = Color::rgba(0.118, 0.118, 0.118, 1.0);
const FG: Color = Color::rgba(0.922, 0.859, 0.698, 1.0);
const MUTED: Color = Color::rgba(0.659, 0.600, 0.518, 1.0);
const YELLOW: Color = Color::rgba(0.843, 0.600, 0.129, 1.0);
const BLUE: Color = Color::rgba(0.271, 0.522, 0.533, 1.0);
const GREEN: Color = Color::rgba(0.596, 0.592, 0.102, 1.0);
const RED: Color = Color::rgba(0.800, 0.141, 0.114, 1.0);

#[derive(Clone)]
struct ViewSession {
    id: u64,
    name: String,
    active: bool,
    windows: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EditMode {
    Search,
    Rename(u64),
}

struct SwitcherState {
    pane: u64,
    sidebar_visible: bool,
    width: f32,
    height: f32,
    sessions: Vec<ViewSession>,
    selected: Option<u64>,
    query: String,
    mode: EditMode,
    scroll: usize,
    request: u64,
    pending: Option<u64>,
    status: String,
    error: bool,
    text_resources: Vec<u64>,
    layout: Option<LayoutContext>,
    workspace_focus: Option<u64>,
}

#[derive(Clone, Deserialize)]
struct LayoutStateMessage {
    #[serde(rename = "type")]
    message_type: serde_json_core::heapless::String<32>,
    revision: u64,
    lease: LayoutLeaseToken,
    visible_panes: serde_json_core::heapless::Vec<u64, 64>,
    #[serde(default)]
    available_windows: serde_json_core::heapless::Vec<AvailableWindow, 64>,
    focused_pane: Option<u64>,
    size: LayoutStateSize,
    #[serde(default)]
    rest: bool,
}

#[derive(Clone, Copy, Deserialize)]
struct AvailableWindow {
    pane: u64,
}

#[derive(Clone, Copy, Deserialize)]
struct LayoutLeaseToken {
    id: u64,
    generation: u64,
}

#[derive(Clone, Copy, Deserialize)]
struct LayoutStateSize {
    w: i32,
    #[serde(rename = "h")]
    _height: i32,
}

#[derive(Clone)]
struct LayoutContext {
    revision: u64,
    lease: LayoutLeaseToken,
    visible_panes: serde_json_core::heapless::Vec<u64, 64>,
    available_windows: serde_json_core::heapless::Vec<AvailableWindow, 64>,
    focused_pane: Option<u64>,
    size: LayoutStateSize,
    rest: bool,
}

#[derive(Deserialize)]
struct SidebarActionMessage {
    #[serde(rename = "type")]
    message_type: serde_json_core::heapless::String<32>,
    action: serde_json_core::heapless::String<32>,
}

impl SwitcherState {
    const fn new() -> Self {
        Self {
            pane: 0,
            sidebar_visible: true,
            width: 320.0,
            height: 720.0,
            sessions: Vec::new(),
            selected: None,
            query: String::new(),
            mode: EditMode::Search,
            scroll: 0,
            request: 0,
            pending: None,
            status: String::new(),
            error: false,
            text_resources: Vec::new(),
            layout: None,
            workspace_focus: None,
        }
    }

    fn next_request(&mut self) -> u64 {
        self.request = self.request.saturating_add(1).max(1);
        self.pending = Some(self.request);
        self.request
    }

    fn filtered_indices(&self) -> Vec<usize> {
        if self.mode != EditMode::Search || self.query.trim().is_empty() {
            return (0..self.sessions.len()).collect();
        }
        let query = self.query.to_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                session
                    .name
                    .to_lowercase()
                    .contains(&query)
                    .then_some(index)
            })
            .collect()
    }

    fn visible_rows(&self) -> usize {
        ((self.height - HEADER_HEIGHT - FOOTER_HEIGHT) / ROW_HEIGHT)
            .floor()
            .max(1.0) as usize
    }

    fn ensure_selection(&mut self) {
        if self
            .selected
            .is_some_and(|id| self.sessions.iter().any(|session| session.id == id))
        {
            return;
        }
        self.selected = self
            .sessions
            .iter()
            .find(|session| session.active)
            .or_else(|| self.sessions.first())
            .map(|session| session.id);
    }

    fn ensure_selected_visible(&mut self) {
        let filtered = self.filtered_indices();
        let Some(position) = self.selected.and_then(|selected| {
            filtered
                .iter()
                .position(|index| self.sessions[*index].id == selected)
        }) else {
            self.scroll = self.scroll.min(filtered.len().saturating_sub(1));
            return;
        };
        if position < self.scroll {
            self.scroll = position;
        } else if position >= self.scroll + self.visible_rows() {
            self.scroll = position + 1 - self.visible_rows();
        }
    }

    fn selected_session(&self) -> Option<&ViewSession> {
        let selected = self.selected?;
        self.sessions.iter().find(|session| session.id == selected)
    }
}

static STATE: Mutex<SwitcherState> = Mutex::new(SwitcherState::new());

struct SessionSwitcher;

impl Guest for SessionSwitcher {
    fn dispatch(event: Event) -> Vec<ActorCommand> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(context)) => {
                if context.instance_id == INIT_ACTOR_ID {
                    vec![
                        ActorCommand::StartActor(ActorStartRequest {
                            id: CHILD_ACTOR_ID.to_string(),
                            path: COMPONENT_FILE.to_string(),
                            capabilities: Vec::new(),
                        }),
                        ActorCommand::LifecycleComplete(Ok(())),
                    ]
                } else {
                    vec![ActorCommand::LifecycleComplete(Ok(()))]
                }
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Presentation(event) => handle_presentation(event),
            Event::Session(event) => handle_session_event(event),
            Event::Mailbox(message) if message.sender == "layout-host" => {
                handle_layout_message(&message.body)
            }
            Event::Mailbox(message) if message.sender == "key-policy" => {
                handle_sidebar_action(&message.body)
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

fn handle_session_event(event: SessionEvent) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match event {
        SessionEvent::Snapshot(snapshot) => apply_snapshot(&mut state, snapshot),
        SessionEvent::CommandCompleted(completion) => {
            apply_completion(&mut state, completion);
        }
    }
    redraw_or_record_error(&mut state);
    Vec::new()
}

fn apply_snapshot(state: &mut SwitcherState, snapshot: SessionSnapshot) {
    state.sessions = snapshot
        .sessions
        .into_iter()
        .map(|session| ViewSession {
            id: session.id,
            name: session.name,
            active: session.active,
            windows: session.windows,
        })
        .collect();
    state.sessions.sort_by_key(|session| session.id);
    state.ensure_selection();
    state.ensure_selected_visible();
}

fn apply_completion(state: &mut SwitcherState, completion: SessionCommandCompleted) {
    if state.pending == Some(completion.request) {
        state.pending = None;
    }
    state.error = !completion.success;
    if completion.success {
        if matches!(
            completion.operation,
            SessionOperation::Activate | SessionOperation::Create | SessionOperation::Rename
        ) && let Some(session) = completion.session
        {
            state.selected = Some(session);
        }
        state.status = match completion.operation {
            SessionOperation::Refresh => "Sessions refreshed".to_string(),
            SessionOperation::Activate => "Switched session".to_string(),
            SessionOperation::Create => "Created session".to_string(),
            SessionOperation::Rename => "Renamed session".to_string(),
            SessionOperation::Close => "Closed session".to_string(),
        };
        if matches!(
            completion.operation,
            SessionOperation::Create | SessionOperation::Rename
        ) {
            state.query.clear();
            state.mode = EditMode::Search;
        }
    } else {
        state.status = completion.message;
    }
}

fn handle_presentation(event: PresentationEvent) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match event {
        PresentationEvent::PaneCreated(created) => {
            state.pane = created.pane;
            state.sidebar_visible = true;
            state.layout = None;
            state.workspace_focus = None;
            state.width = created.width.max(240.0);
            state.height = created.height.max(240.0);
            redraw_or_record_error(&mut state);
            let request = state.next_request();
            vec![
                ActorCommand::SendHostMessage(br#"{"type":"claim-root-layout"}"#.to_vec()),
                session_command(request, SessionOperation::Refresh, None, None),
            ]
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            state.width = resized.width.max(240.0);
            state.height = resized.height.max(240.0);
            state.ensure_selected_visible();
            redraw_or_record_error(&mut state);
            Vec::new()
        }
        PresentationEvent::PaneFocused(focused) if focused.pane == state.pane => {
            let pane = state.pane;
            let workspace_focus = state.workspace_focus;
            if let Some(layout) = state.layout.as_mut() {
                layout.focused_pane = if focused.focused {
                    Some(pane)
                } else {
                    workspace_focus
                };
            }
            Vec::new()
        }
        PresentationEvent::PointerButton(button) if button.pane == state.pane && button.pressed => {
            let commands = handle_pointer_button(&mut state, button.x, button.y, button.button);
            redraw_or_record_error(&mut state);
            commands
        }
        PresentationEvent::PointerScroll(scroll) if scroll.pane == state.pane => {
            let filtered_len = state.filtered_indices().len();
            let max_scroll = filtered_len.saturating_sub(state.visible_rows());
            if scroll.vertical < 0.0 {
                state.scroll = (state.scroll + 1).min(max_scroll);
            } else if scroll.vertical > 0.0 {
                state.scroll = state.scroll.saturating_sub(1);
            }
            redraw_or_record_error(&mut state);
            Vec::new()
        }
        PresentationEvent::Key(key) if key.pane == state.pane && key.pressed => {
            let commands = handle_key(&mut state, &key.logical_key);
            redraw_or_record_error(&mut state);
            commands
        }
        PresentationEvent::Text(text) if text.pane == state.pane => {
            for character in text
                .text
                .chars()
                .filter(|character| !character.is_control())
            {
                state.query.push(character);
            }
            state.scroll = 0;
            select_first_filtered(&mut state);
            redraw_or_record_error(&mut state);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn handle_layout_message(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((message, consumed)) = serde_json_core::from_slice::<LayoutStateMessage>(bytes) else {
        return Vec::new();
    };
    if consumed != bytes.len() || message.message_type.as_str() != "layout-state" {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if state.pane == 0 {
        return Vec::new();
    }
    if message.focused_pane != Some(state.pane) {
        state.workspace_focus = message.focused_pane;
    }
    state.layout = Some(LayoutContext {
        revision: message.revision,
        lease: message.lease,
        visible_panes: message.visible_panes,
        available_windows: message.available_windows,
        focused_pane: message.focused_pane,
        size: message.size,
        rest: message.rest,
    });
    publish_layout(&state, true, message.focused_pane)
}

fn handle_sidebar_action(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((message, consumed)) = serde_json_core::from_slice::<SidebarActionMessage>(bytes) else {
        return Vec::new();
    };
    if consumed != bytes.len() || message.message_type.as_str() != "sidebar-action" {
        return Vec::new();
    }

    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let pane = state.pane;
    if state.layout.is_none() {
        return Vec::new();
    }
    let requested_focus = match message.action.as_str() {
        "show" => {
            state.sidebar_visible = true;
            Some(pane)
        }
        "hide" => {
            state.sidebar_visible = false;
            state.workspace_focus
        }
        "toggle" if !state.sidebar_visible => {
            state.sidebar_visible = true;
            Some(pane)
        }
        "toggle" => {
            state.sidebar_visible = false;
            state.workspace_focus
        }
        _ => return Vec::new(),
    };
    if let Some(layout) = state.layout.as_mut() {
        layout.focused_pane = requested_focus;
    }
    publish_layout(&state, false, requested_focus)
}

fn publish_layout(
    state: &SwitcherState,
    allow_delegation: bool,
    requested_focus: Option<u64>,
) -> Vec<ActorCommand> {
    let Some(message) = state.layout.as_ref() else {
        return Vec::new();
    };
    let pane = state.pane;
    let delegated = message
        .visible_panes
        .iter()
        .copied()
        .filter(|visible| *visible != pane)
        .chain(
            message
                .available_windows
                .iter()
                .map(|window| window.pane),
        )
        .fold(Vec::new(), |mut panes, candidate| {
            if !panes.contains(&candidate) {
                panes.push(candidate);
            }
            panes
        });
    let needs_rest = allow_delegation && !message.rest && !delegated.is_empty();
    let has_rest = message.rest || !delegated.is_empty();
    let pane_visible = state.sidebar_visible && message.visible_panes.contains(&pane);
    let width = f64::from(message.size.w.max(1));
    let sidebar_fraction = if has_rest && pane_visible {
        (SIDEBAR_WIDTH / width).clamp(0.2, 0.45)
    } else if has_rest {
        0.0
    } else {
        1.0
    };
    let sidebar_offset = 1.0 - 2.0 * sidebar_fraction;
    let sidebar_size = 2.0 * sidebar_fraction;
    let rest_size = 2.0 * (1.0 - sidebar_fraction);

    let mut commands = Vec::new();
    if needs_rest {
        let panes = delegated
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        commands.push(ActorCommand::SendHostMessage(
            format!(
                "{{\"type\":\"delegate-layout-rest\",\"lease\":{{\"id\":{},\"generation\":{}}},\"revision\":{},\"actor\":\"workspace\",\"panes\":[{}]}}",
                message.lease.id, message.lease.generation, message.revision, panes
            )
            .into_bytes(),
        ));
    }

    let layout_item = if pane_visible {
        format!(
            "{{\"id\":{},\"kind\":{{\"type\":\"pane\",\"pane\":{}}},\"z\":1,\"transform\":{{\"offset\":{{\"u\":{},\"v\":-1}},\"size\":{{\"u\":{},\"v\":2}}}},\"input\":\"pane\"}}",
            pane, pane, sidebar_offset, sidebar_size
        )
    } else {
        String::new()
    };
    let rest = if has_rest {
        format!(
            ",\"rest\":{{\"z\":0,\"transform\":{{\"offset\":{{\"u\":-1,\"v\":-1}},\"size\":{{\"u\":{},\"v\":2}}}}}}",
            rest_size
        )
    } else {
        String::new()
    };
    let focus = requested_focus.map_or_else(|| "null".to_string(), |focus| focus.to_string());
    commands.push(ActorCommand::SendHostMessage(
        format!(
            "{{\"type\":\"set-resolved-layout\",\"lease\":{{\"id\":{},\"generation\":{}}},\"revision\":{},\"layout\":{{\"items\":[{}]}}{},\"focus\":{}}}",
            message.lease.id,
            message.lease.generation,
            message.revision,
            layout_item,
            rest,
            focus
        )
        .into_bytes(),
    ));
    commands
}

fn handle_pointer_button(
    state: &mut SwitcherState,
    x: f32,
    y: f32,
    button: u32,
) -> Vec<ActorCommand> {
    if y < 96.0 && x >= state.width - 76.0 && button == LEFT_BUTTON {
        return create_from_query(state);
    }
    if y < HEADER_HEIGHT || y >= state.height - FOOTER_HEIGHT {
        return Vec::new();
    }
    let filtered = state.filtered_indices();
    let row = ((y - HEADER_HEIGHT) / ROW_HEIGHT).floor().max(0.0) as usize;
    let Some(index) = filtered.get(state.scroll + row).copied() else {
        return Vec::new();
    };
    let session = state.sessions[index].clone();
    state.selected = Some(session.id);
    if button == RIGHT_BUTTON {
        begin_rename(state, &session);
        return Vec::new();
    }
    if button != LEFT_BUTTON {
        return Vec::new();
    }
    if x >= state.width - 48.0 {
        return close_selected(state);
    }
    let request = state.next_request();
    vec![session_command(
        request,
        SessionOperation::Activate,
        Some(session.id),
        None,
    )]
}

fn handle_key(state: &mut SwitcherState, logical_key: &str) -> Vec<ActorCommand> {
    match logical_key {
        "ArrowUp" => {
            move_selection(state, -1);
            Vec::new()
        }
        "ArrowDown" => {
            move_selection(state, 1);
            Vec::new()
        }
        "Enter" => submit(state),
        "Escape" => {
            state.query.clear();
            state.mode = EditMode::Search;
            state.error = false;
            state.status.clear();
            state.scroll = 0;
            state.ensure_selection();
            Vec::new()
        }
        "Backspace" => {
            state.query.pop();
            state.scroll = 0;
            select_first_filtered(state);
            Vec::new()
        }
        "Delete" if state.mode == EditMode::Search && state.query.is_empty() => {
            close_selected(state)
        }
        "F2" => {
            if let Some(session) = state.selected_session().cloned() {
                begin_rename(state, &session);
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn submit(state: &mut SwitcherState) -> Vec<ActorCommand> {
    if let EditMode::Rename(session) = state.mode {
        let name = state.query.trim().to_string();
        if name.is_empty() {
            state.error = true;
            state.status = "A session name cannot be empty".to_string();
            return Vec::new();
        }
        let request = state.next_request();
        return vec![session_command(
            request,
            SessionOperation::Rename,
            Some(session),
            Some(name),
        )];
    }
    let filtered = state.filtered_indices();
    if let Some(selected) = state.selected
        && filtered
            .iter()
            .any(|index| state.sessions[*index].id == selected)
    {
        let request = state.next_request();
        return vec![session_command(
            request,
            SessionOperation::Activate,
            Some(selected),
            None,
        )];
    }
    create_from_query(state)
}

fn create_from_query(state: &mut SwitcherState) -> Vec<ActorCommand> {
    let name = state.query.trim().to_string();
    if name.is_empty() {
        state.error = true;
        state.status = "Type a name to create a session".to_string();
        return Vec::new();
    }
    let request = state.next_request();
    vec![session_command(
        request,
        SessionOperation::Create,
        None,
        Some(name),
    )]
}

fn close_selected(state: &mut SwitcherState) -> Vec<ActorCommand> {
    let Some(session) = state.selected else {
        return Vec::new();
    };
    if state.sessions.len() <= 1 {
        state.error = true;
        state.status = "The last session cannot be closed".to_string();
        return Vec::new();
    }
    let request = state.next_request();
    vec![session_command(
        request,
        SessionOperation::Close,
        Some(session),
        None,
    )]
}

fn begin_rename(state: &mut SwitcherState, session: &ViewSession) {
    state.mode = EditMode::Rename(session.id);
    state.query.clone_from(&session.name);
    state.status = "Rename: Enter to save, Escape to cancel".to_string();
    state.error = false;
}

fn move_selection(state: &mut SwitcherState, delta: isize) {
    let filtered = state.filtered_indices();
    if filtered.is_empty() {
        state.selected = None;
        return;
    }
    let current = state
        .selected
        .and_then(|selected| {
            filtered
                .iter()
                .position(|index| state.sessions[*index].id == selected)
        })
        .unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(filtered.len() - 1)
    };
    state.selected = Some(state.sessions[filtered[next]].id);
    state.ensure_selected_visible();
}

fn select_first_filtered(state: &mut SwitcherState) {
    let filtered = state.filtered_indices();
    state.selected = filtered.first().map(|index| state.sessions[*index].id);
}

fn session_command(
    request: u64,
    operation: SessionOperation,
    session: Option<u64>,
    name: Option<String>,
) -> ActorCommand {
    ActorCommand::SessionCommand(SessionCommand {
        request,
        operation,
        session,
        name,
    })
}

fn redraw_or_record_error(state: &mut SwitcherState) {
    if state.pane == 0 {
        return;
    }
    if let Err(error) = redraw(state) {
        state.error = true;
        state.status = error;
    }
}

fn redraw(state: &mut SwitcherState) -> Result<(), String> {
    let mut packet = Packet::new();
    let mut resources = Vec::new();
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, state.height),
        color: BG,
    });

    add_text(
        state,
        &mut packet,
        &mut resources,
        "Sessions",
        22.0,
        FG,
        SIDE_MARGIN,
        12.0,
        state.width - 28.0,
    )?;

    let input_color = if state.mode == EditMode::Search {
        PANEL
    } else {
        BLUE
    };
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(SIDE_MARGIN, 50.0, state.width - 104.0, 40.0),
        radius: 7.0,
        color: PANEL_DARK,
    });
    packet.push(Command::StrokeRoundedRect {
        rect: Rect::new(SIDE_MARGIN, 50.0, state.width - 104.0, 40.0),
        radius: 7.0,
        color: input_color,
        width: 1.5,
    });
    let prompt = if state.query.is_empty() {
        if state.mode == EditMode::Search {
            "Search or create…".to_string()
        } else {
            "New name…".to_string()
        }
    } else {
        format!("{}▏", state.query)
    };
    add_text(
        state,
        &mut packet,
        &mut resources,
        &prompt,
        15.0,
        if state.query.is_empty() { MUTED } else { FG },
        SIDE_MARGIN + 10.0,
        58.0,
        state.width - 124.0,
    )?;

    packet.push(Command::FillRoundedRect {
        rect: Rect::new(state.width - 78.0, 50.0, 64.0, 40.0),
        radius: 7.0,
        color: if state.pending.is_some() {
            PANEL
        } else {
            YELLOW
        },
    });
    add_text(
        state,
        &mut packet,
        &mut resources,
        if state.pending.is_some() {
            "…"
        } else {
            "+ New"
        },
        14.0,
        PANEL_DARK,
        state.width - 68.0,
        59.0,
        48.0,
    )?;

    let filtered = state.filtered_indices();
    let visible = state.visible_rows();
    packet.push(Command::Save);
    packet.push(Command::ClipRect(Rect::new(
        0.0,
        HEADER_HEIGHT,
        state.width,
        state.height - HEADER_HEIGHT - FOOTER_HEIGHT,
    )));
    for (visible_row, index) in filtered
        .iter()
        .skip(state.scroll)
        .take(visible)
        .copied()
        .enumerate()
    {
        let session = state.sessions[index].clone();
        let y = HEADER_HEIGHT + visible_row as f32 * ROW_HEIGHT;
        let selected = state.selected == Some(session.id);
        if session.active || selected {
            packet.push(Command::FillRoundedRect {
                rect: Rect::new(SIDE_MARGIN, y + 3.0, state.width - 28.0, ROW_HEIGHT - 6.0),
                radius: 6.0,
                color: if session.active {
                    Color::rgba(BLUE.r, BLUE.g, BLUE.b, 0.46)
                } else {
                    Color::rgba(PANEL.r, PANEL.g, PANEL.b, 0.72)
                },
            });
        }
        if selected {
            packet.push(Command::StrokeRoundedRect {
                rect: Rect::new(SIDE_MARGIN, y + 3.0, state.width - 28.0, ROW_HEIGHT - 6.0),
                radius: 6.0,
                color: YELLOW,
                width: 1.5,
            });
        }
        let display_name = truncate(&session.name, 28);
        add_text(
            state,
            &mut packet,
            &mut resources,
            &display_name,
            16.0,
            FG,
            SIDE_MARGIN + 10.0,
            y + 8.0,
            state.width - 146.0,
        )?;
        let count = if session.windows == 1 {
            "1 window".to_string()
        } else {
            format!("{} windows", session.windows)
        };
        add_text(
            state,
            &mut packet,
            &mut resources,
            &count,
            12.0,
            if session.active { GREEN } else { MUTED },
            state.width - 116.0,
            y + 11.0,
            70.0,
        )?;
        add_text(
            state,
            &mut packet,
            &mut resources,
            "×",
            18.0,
            RED,
            state.width - 38.0,
            y + 7.0,
            20.0,
        )?;
    }
    packet.push(Command::Restore);

    if filtered.len() > visible {
        let track_height = state.height - HEADER_HEIGHT - FOOTER_HEIGHT - 12.0;
        let thumb_height = (track_height * visible as f32 / filtered.len() as f32).max(24.0);
        let max_scroll = filtered.len().saturating_sub(visible).max(1);
        let thumb_y = HEADER_HEIGHT
            + 6.0
            + (track_height - thumb_height) * state.scroll as f32 / max_scroll as f32;
        packet.push(Command::FillRoundedRect {
            rect: Rect::new(state.width - 5.0, thumb_y, 3.0, thumb_height),
            radius: 1.5,
            color: MUTED,
        });
    }

    packet.push(Command::FillRect {
        rect: Rect::new(
            0.0,
            state.height - FOOTER_HEIGHT,
            state.width,
            FOOTER_HEIGHT,
        ),
        color: PANEL_DARK,
    });
    let footer = if !state.status.is_empty() {
        state.status.clone()
    } else if filtered.is_empty() {
        "No match — Enter creates it".to_string()
    } else {
        "Enter open  ·  F2 rename  ·  Delete close".to_string()
    };
    add_text(
        state,
        &mut packet,
        &mut resources,
        &truncate(&footer, 48),
        12.0,
        if state.error { RED } else { MUTED },
        SIDE_MARGIN,
        state.height - 28.0,
        state.width - 28.0,
    )?;

    let bytes = packet.encode().map_err(|error| error.to_string())?;
    host::graphics_submit_draw_packet(state.pane, &bytes)
        .map_err(|error| format!("submit session switcher: {error:?}"))?;
    for handle in state.text_resources.drain(..) {
        let _ = host::graphics_drop_resource(state.pane, handle);
    }
    state.text_resources = resources;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_text(
    state: &SwitcherState,
    packet: &mut Packet,
    resources: &mut Vec<u64>,
    text: &str,
    size: f32,
    color: Color,
    x: f32,
    y: f32,
    max_width: f32,
) -> Result<(), String> {
    let layout = host::graphics_layout_text(
        state.pane,
        text,
        host::TextLayoutOptions {
            font_size: size,
            max_width: max_width.max(1.0),
            line_height: size + 4.0,
            red: color.r,
            green: color.g,
            blue: color.b,
            alpha: color.a,
        },
    )
    .map_err(|error| format!("layout session switcher text: {error:?}"))?;
    resources.push(layout.handle);
    packet.push(Command::DrawTextLayout {
        layout: layout.handle,
        x,
        y,
    });
    Ok(())
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut characters = text.chars();
    let prefix = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

export!(SessionSwitcher with_types_in self);
