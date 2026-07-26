wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use brood::plugin::host;
use brood_graphics::{Color, Command, Packet, Rect};
use exports::brood::plugin::actor::{
    ActorStartRequest, ChildEvent, Command as ActorCommand, Event, Guest, LifecycleEvent,
    LocalActorMessage, PresentationEvent, SessionActorMessage, SessionCommand,
    SessionCommandCompleted, SessionEvent, SessionOperation, SessionSnapshot,
};
use serde::{Deserialize, Serialize};

const INIT_ACTOR_ID: &str = "brood.client-init";
const CHILD_ACTOR_ID: &str = "session-switcher";
const PICKER_ACTOR_ID: &str = "picker";
const COMPONENT_FILE: &str = "brood_session_switcher_plugin.component.wasm";
const PICKER_COMPONENT_FILE: &str = "brood_picker_plugin.component.wasm";
const CODING_AGENT_COMPONENT_FILE: &str = "brood_coding_agent_plugin.component.wasm";
const DIFF_VIEWER_COMPONENT_FILE: &str = "brood_diff_viewer_plugin.component.wasm";
const TERMINAL_UI_COMPONENT_FILE: &str = "brood_terminal_ui_plugin.component.wasm";
const TERMINAL_UI_ID: &str = "terminal-ui";
const CODING_AGENT_UI_ID: &str = "coding-agent-ui";
const DIFF_VIEWER_ID_PREFIX: &str = "diff-viewer-s";
const LEFT_BUTTON: u32 = 0x110;
const RIGHT_BUTTON: u32 = 0x111;
const HEADER_HEIGHT: f32 = 104.0;
const FOOTER_HEIGHT: f32 = 58.0;
const ROW_HEIGHT: f32 = 46.0;
const PROJECT_ROW_HEIGHT: f32 = 38.0;
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
    project_id: String,
    project_name: String,
    branch: Option<String>,
    worktree: bool,
    git: bool,
}

#[derive(Clone, Eq, PartialEq)]
enum EditMode {
    Search,
    Rename(u64),
    CreateWorktree(u64),
}

#[derive(Clone)]
struct ProjectGroup {
    id: String,
    name: String,
    source_session: u64,
    sessions: Vec<usize>,
    git: bool,
}

#[derive(Clone)]
enum ListItem {
    Project(ProjectGroup),
    Session(usize),
}

struct SwitcherState {
    pane: u64,
    session_id: u64,
    sidebar_visible: bool,
    width: f32,
    height: f32,
    sessions: Vec<ViewSession>,
    collapsed_projects: BTreeSet<String>,
    selected: Option<u64>,
    query: String,
    mode: EditMode,
    scroll: usize,
    request: u64,
    pending: Option<u64>,
    status: String,
    error: bool,
    next_coding_agent_ui: u64,
    coding_agent_uis: Vec<String>,
    coding_agent_targets: BTreeMap<String, String>,
    agents: Vec<WireAgent>,
    agent_picker_pending: bool,
    diff_viewers: BTreeSet<String>,
    terminal_uis: BTreeMap<String, u64>,
    text_resources: Vec<u64>,
    layout: Option<LayoutContext>,
    workspace_focus: Option<u64>,
    picker_pane: Option<u64>,
    picker_visible: bool,
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
    height: i32,
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

#[derive(Deserialize)]
struct CodingAgentActionMessage {
    kind: serde_json_core::heapless::String<32>,
    action: serde_json_core::heapless::String<32>,
}

#[derive(Deserialize)]
struct TerminalActionMessage {
    kind: serde_json_core::heapless::String<32>,
    action: serde_json_core::heapless::String<32>,
}

#[derive(Deserialize)]
struct TerminalLifecycleMessage {
    kind: serde_json_core::heapless::String<32>,
    #[serde(default)]
    session_id: u64,
    terminal: Option<serde_json_core::heapless::String<64>>,
}

#[derive(Clone, Deserialize)]
struct AgentRegistryMessage {
    kind: serde_json_core::heapless::String<32>,
    #[serde(default)]
    session_id: u64,
    #[serde(default)]
    agents: Vec<WireAgent>,
}

#[derive(Clone, Deserialize)]
struct WireAgent {
    id: String,
    status: String,
    model: String,
    reasoning: String,
}

#[derive(Deserialize)]
struct PickerLayoutMessage {
    #[serde(rename = "type")]
    message_type: serde_json_core::heapless::String<32>,
    pane: u64,
    visible: bool,
}

#[derive(Deserialize)]
struct PickerReplyKind {
    #[serde(rename = "type")]
    message_type: serde_json_core::heapless::String<32>,
}

#[derive(Deserialize)]
struct PickerSelection {
    #[serde(rename = "type")]
    _message_type: serde_json_core::heapless::String<32>,
    result: SessionPickerResult,
}

#[derive(Deserialize)]
struct SessionPickerResult {
    value: SessionPickerValue,
}

#[derive(Deserialize)]
struct SessionPickerValue {
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    agent: Option<String>,
}

#[derive(Deserialize)]
struct PickerErrorMessage {
    #[serde(rename = "type")]
    _message_type: serde_json_core::heapless::String<32>,
    message: serde_json_core::heapless::String<256>,
}

#[derive(Serialize)]
struct SessionPickerRequest<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    title: &'static str,
    placeholder: &'static str,
    items: Vec<SessionPickerItem<'a>>,
}

#[derive(Serialize)]
struct SessionPickerItem<'a> {
    label: &'a str,
    detail: String,
    value: SessionPickerValueWire,
}

#[derive(Serialize)]
struct SessionPickerValueWire {
    session: u64,
}

#[derive(Serialize)]
struct AgentPickerRequest<'a> {
    #[serde(rename = "type")]
    message_type: &'static str,
    title: &'static str,
    placeholder: &'static str,
    items: Vec<AgentPickerItem<'a>>,
}

#[derive(Serialize)]
struct AgentPickerItem<'a> {
    label: &'a str,
    detail: String,
    value: AgentPickerValue<'a>,
}

#[derive(Serialize)]
struct AgentPickerValue<'a> {
    agent: &'a str,
}

impl SwitcherState {
    const fn new() -> Self {
        Self {
            pane: 0,
            session_id: 0,
            sidebar_visible: true,
            width: 320.0,
            height: 720.0,
            sessions: Vec::new(),
            collapsed_projects: BTreeSet::new(),
            selected: None,
            query: String::new(),
            mode: EditMode::Search,
            scroll: 0,
            request: 0,
            pending: None,
            status: String::new(),
            error: false,
            next_coding_agent_ui: 2,
            coding_agent_uis: Vec::new(),
            coding_agent_targets: BTreeMap::new(),
            agents: Vec::new(),
            agent_picker_pending: false,
            diff_viewers: BTreeSet::new(),
            terminal_uis: BTreeMap::new(),
            text_resources: Vec::new(),
            layout: None,
            workspace_focus: None,
            picker_pane: None,
            picker_visible: false,
        }
    }

    fn next_request(&mut self) -> u64 {
        self.request = self.request.saturating_add(1).max(1);
        self.pending = Some(self.request);
        self.error = false;
        self.status.clear();
        self.request
    }

    fn project_groups(&self) -> Vec<ProjectGroup> {
        let mut groups = Vec::<ProjectGroup>::new();
        for (index, session) in self.sessions.iter().enumerate() {
            if let Some(group) = groups
                .iter_mut()
                .find(|group| group.id == session.project_id)
            {
                group.sessions.push(index);
                let source_is_worktree = self
                    .sessions
                    .iter()
                    .find(|candidate| candidate.id == group.source_session)
                    .is_some_and(|candidate| candidate.worktree);
                if source_is_worktree && !session.worktree {
                    group.source_session = session.id;
                }
            } else {
                groups.push(ProjectGroup {
                    id: session.project_id.clone(),
                    name: session.project_name.clone(),
                    source_session: session.id,
                    sessions: vec![index],
                    git: session.git,
                });
            }
        }
        groups.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        groups
    }

    fn list_items(&self) -> Vec<ListItem> {
        let query = (self.mode == EditMode::Search)
            .then(|| self.query.trim().to_lowercase())
            .filter(|query| !query.is_empty());
        let mut items = Vec::new();
        for mut group in self.project_groups() {
            let project_matches = query.as_ref().is_some_and(|query| {
                group.name.to_lowercase().contains(query) || group.id.to_lowercase().contains(query)
            });
            if let Some(query) = query.as_ref()
                && !project_matches
            {
                group.sessions.retain(|index| {
                    let session = &self.sessions[*index];
                    session.name.to_lowercase().contains(query)
                        || session
                            .branch
                            .as_ref()
                            .is_some_and(|branch| branch.to_lowercase().contains(query))
                });
            }
            if group.sessions.is_empty() {
                continue;
            }
            let expanded = query.is_some() || !self.collapsed_projects.contains(&group.id);
            items.push(ListItem::Project(group.clone()));
            if expanded {
                items.extend(group.sessions.into_iter().map(ListItem::Session));
            }
        }
        items
    }

    fn visible_item_count(&self, items: &[ListItem], scroll: usize) -> usize {
        let available = (self.height - HEADER_HEIGHT - FOOTER_HEIGHT).max(PROJECT_ROW_HEIGHT);
        let mut used = 0.0;
        items
            .iter()
            .skip(scroll)
            .take_while(|item| {
                let height = item_height(item);
                if used + height > available && used > 0.0 {
                    return false;
                }
                used += height;
                true
            })
            .count()
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
        let items = self.list_items();
        let Some(position) = self.selected.and_then(|selected| {
            items
                .iter()
                .position(|item| matches!(item, ListItem::Session(index) if self.sessions[*index].id == selected))
        }) else {
            self.scroll = self.scroll.min(items.len().saturating_sub(1));
            return;
        };
        if position < self.scroll {
            self.scroll = position;
        } else {
            let visible = self.visible_item_count(&items, self.scroll).max(1);
            if position >= self.scroll + visible {
                self.scroll = position + 1 - visible;
            }
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
                    if let Ok(mut state) = STATE.lock() {
                        state.session_id = context.session_id.unwrap_or_default();
                    }
                    vec![
                        ActorCommand::StartActor(ActorStartRequest {
                            id: CHILD_ACTOR_ID.to_string(),
                            path: COMPONENT_FILE.to_string(),
                            capabilities: Vec::new(),
                        }),
                        ActorCommand::StartActor(ActorStartRequest {
                            id: PICKER_ACTOR_ID.to_string(),
                            path: PICKER_COMPONENT_FILE.to_string(),
                            capabilities: Vec::new(),
                        }),
                        ActorCommand::StartActor(ActorStartRequest {
                            id: TERMINAL_UI_ID.to_string(),
                            path: TERMINAL_UI_COMPONENT_FILE.to_string(),
                            capabilities: Vec::new(),
                        }),
                        subscribe_agent_registry(context.session_id.unwrap_or_default()),
                        ActorCommand::LifecycleComplete(Ok(())),
                    ]
                } else {
                    vec![ActorCommand::LifecycleComplete(Ok(()))]
                }
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Child(ChildEvent::Started(started))
                if started.id.starts_with(DIFF_VIEWER_ID_PREFIX) =>
            {
                if let Ok(mut state) = STATE.lock() {
                    state.diff_viewers.insert(started.id);
                }
                Vec::new()
            }
            Event::Child(ChildEvent::Started(started))
                if started.id == TERMINAL_UI_ID || started.id.starts_with("terminal-ui-s") =>
            {
                if let Ok(mut state) = STATE.lock() {
                    let session = actor_session(&started.id).unwrap_or(state.session_id);
                    state.terminal_uis.insert(started.id, session);
                }
                Vec::new()
            }
            Event::Child(ChildEvent::Started(started))
                if started.id == CODING_AGENT_UI_ID
                    || started.id.starts_with("coding-agent-ui-") =>
            {
                let Ok(mut state) = STATE.lock() else {
                    return Vec::new();
                };
                if !state.coding_agent_uis.contains(&started.id) {
                    state.coding_agent_uis.push(started.id.clone());
                }
                let target = state.coding_agent_targets.remove(&started.id);
                vec![bind_coding_agent_ui(
                    &started.id,
                    actor_session(&started.id).unwrap_or(state.session_id),
                    target.as_deref(),
                )]
            }
            Event::Child(ChildEvent::Stopped(id)) => {
                let Ok(mut state) = STATE.lock() else {
                    return Vec::new();
                };
                state.coding_agent_uis.retain(|actor| actor != &id);
                state.coding_agent_targets.remove(&id);
                state.diff_viewers.remove(&id);
                state
                    .terminal_uis
                    .remove(&id)
                    .and_then(|session| stop_terminal_command(session, &id))
                    .into_iter()
                    .collect()
            }
            Event::Child(ChildEvent::Failed(failed)) => {
                let Ok(mut state) = STATE.lock() else {
                    return Vec::new();
                };
                state.coding_agent_uis.retain(|actor| actor != &failed.id);
                state.coding_agent_targets.remove(&failed.id);
                state.diff_viewers.remove(&failed.id);
                let stop = state
                    .terminal_uis
                    .remove(&failed.id)
                    .and_then(|session| stop_terminal_command(session, &failed.id));
                state.error = true;
                state.status = format!("Plugin pane failed: {}", failed.message);
                stop.into_iter().collect()
            }
            Event::Presentation(event) => handle_presentation(event),
            Event::Session(event) => handle_session_event(event),
            Event::Mailbox(message) if message.sender == "layout-host" => {
                handle_layout_message(&message.body)
            }
            Event::Mailbox(message) if message.sender == "key-policy" => {
                handle_key_policy_action(&message.body)
            }
            Event::Mailbox(message) if message.sender == PICKER_ACTOR_ID => {
                handle_picker_message(&message.body)
            }
            Event::Mailbox(message) if message.sender.starts_with("session:") => {
                handle_session_message(&message.body)
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
    let commands = match event {
        SessionEvent::Snapshot(snapshot) => {
            let active_changed = state.session_id != snapshot.active;
            state.session_id = snapshot.active;
            apply_snapshot(&mut state, snapshot);
            if active_changed {
                state.agents.clear();
                vec![subscribe_agent_registry(state.session_id)]
            } else {
                Vec::new()
            }
        }
        SessionEvent::CommandCompleted(completion) => {
            apply_completion(&mut state, completion);
            Vec::new()
        }
    };
    redraw_or_record_error(&mut state);
    commands
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
            project_id: session.project_id,
            project_name: session.project_name,
            branch: session.branch,
            worktree: session.worktree,
            git: session.git,
        })
        .collect();
    state.sessions.sort_by(|left, right| {
        left.project_name
            .to_lowercase()
            .cmp(&right.project_name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    let projects = state
        .sessions
        .iter()
        .map(|session| session.project_id.clone())
        .collect::<BTreeSet<_>>();
    state
        .collapsed_projects
        .retain(|project| projects.contains(project));
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
            SessionOperation::Activate
                | SessionOperation::Create
                | SessionOperation::Rename
                | SessionOperation::CreateWorktree
        ) && let Some(session) = completion.session
        {
            state.selected = Some(session);
        }
        state.status = match completion.operation {
            SessionOperation::Refresh => "Sessions refreshed".to_string(),
            SessionOperation::Activate => "Switched session".to_string(),
            SessionOperation::Create => "Created session".to_string(),
            SessionOperation::CreateWorktree => "Created worktree session".to_string(),
            SessionOperation::Rename => "Renamed session".to_string(),
            SessionOperation::Close => "Closed session".to_string(),
        };
        if matches!(
            completion.operation,
            SessionOperation::Create | SessionOperation::CreateWorktree | SessionOperation::Rename
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
            let items = state.list_items();
            let visible = state.visible_item_count(&items, state.scroll);
            let max_scroll = items.len().saturating_sub(visible.max(1));
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
            if state.mode == EditMode::Search {
                state.scroll = 0;
                select_first_filtered(&mut state);
            }
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
    if message.focused_pane != Some(state.pane) && message.focused_pane != state.picker_pane {
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

fn handle_key_policy_action(bytes: &[u8]) -> Vec<ActorCommand> {
    if let Ok((message, consumed)) = serde_json_core::from_slice::<CodingAgentActionMessage>(bytes)
        && consumed == bytes.len()
        && message.kind.as_str() == "coding-agent-action"
        && message.action.as_str() == "picker"
    {
        let Ok(mut state) = STATE.lock() else {
            return Vec::new();
        };
        if state.agents.is_empty() {
            state.agent_picker_pending = true;
            return vec![subscribe_agent_registry(state.session_id)];
        }
        return open_agent_picker(&mut state);
    }
    if let Ok((message, consumed)) = serde_json_core::from_slice::<CodingAgentActionMessage>(bytes)
        && consumed == bytes.len()
        && message.kind.as_str() == "diff-viewer-action"
        && message.action.as_str() == "toggle"
    {
        let Ok(mut state) = STATE.lock() else {
            return Vec::new();
        };
        let id = format!("{DIFF_VIEWER_ID_PREFIX}{}", state.session_id);
        return if state.diff_viewers.contains(&id) {
            vec![ActorCommand::SendLocalMessage(LocalActorMessage {
                actor: id,
                body: br#"{"kind":"focus"}"#.to_vec(),
            })]
        } else {
            state.diff_viewers.insert(id.clone());
            vec![start_diff_viewer(&id)]
        };
    }
    if let Ok((message, consumed)) = serde_json_core::from_slice::<TerminalActionMessage>(bytes)
        && consumed == bytes.len()
        && message.kind.as_str() == "terminal-action"
        && message.action.as_str() == "new"
    {
        let Ok(state) = STATE.lock() else {
            return Vec::new();
        };
        if state.session_id == 0 {
            return Vec::new();
        }
        return vec![ActorCommand::SendSessionMessage(SessionActorMessage {
            session_id: state.session_id,
            actor: "init".to_string(),
            body: br#"{"kind":"create-terminal"}"#.to_vec(),
        })];
    }
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
        "picker" => return open_session_picker(&mut state),
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

fn handle_session_message(bytes: &[u8]) -> Vec<ActorCommand> {
    if let Ok((message, consumed)) = serde_json_core::from_slice::<AgentRegistryMessage>(bytes)
        && consumed == bytes.len()
        && message.kind.as_str() == "registry"
    {
        let Ok(mut state) = STATE.lock() else {
            return Vec::new();
        };
        if message.session_id != state.session_id {
            return Vec::new();
        }
        state.agents = message.agents;
        if state.agent_picker_pending && !state.agents.is_empty() {
            return open_agent_picker(&mut state);
        }
        return Vec::new();
    }
    handle_terminal_lifecycle(bytes)
}

fn handle_terminal_lifecycle(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((message, consumed)) = serde_json_core::from_slice::<TerminalLifecycleMessage>(bytes)
    else {
        return Vec::new();
    };
    if consumed != bytes.len() || message.kind.as_str() != "terminal-started" {
        return Vec::new();
    }
    let Some(terminal) = message.terminal else {
        return Vec::new();
    };
    let Some(suffix) = terminal.as_str().strip_prefix("terminal-session-") else {
        return Vec::new();
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let session_id = if message.session_id == 0 {
        state.session_id
    } else {
        message.session_id
    };
    let id = format!("terminal-ui-s{session_id}-{suffix}");
    state.terminal_uis.insert(id.clone(), session_id);
    vec![ActorCommand::StartActor(ActorStartRequest {
        id,
        path: TERMINAL_UI_COMPONENT_FILE.to_string(),
        capabilities: Vec::new(),
    })]
}

fn handle_picker_message(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((kind, consumed)) = serde_json_core::from_slice::<PickerReplyKind>(bytes) else {
        return Vec::new();
    };
    if consumed != bytes.len() {
        return Vec::new();
    }
    match kind.message_type.as_str() {
        "picker-pane" => handle_picker_layout_message(bytes),
        "picker-created" => {
            if let Ok(mut state) = STATE.lock() {
                state.status = "Session picker opened".to_string();
                state.error = false;
                redraw_or_record_error(&mut state);
            }
            Vec::new()
        }
        "picker-selected" => handle_picker_selection(bytes),
        "picker-error" => {
            if let Ok((message, consumed)) =
                serde_json_core::from_slice::<PickerErrorMessage>(bytes)
                && consumed == bytes.len()
                && let Ok(mut state) = STATE.lock()
            {
                state.status = message.message.as_str().to_string();
                state.error = true;
                redraw_or_record_error(&mut state);
            }
            Vec::new()
        }
        "picker-cancelled" => Vec::new(),
        _ => Vec::new(),
    }
}

fn handle_picker_layout_message(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((message, consumed)) = serde_json_core::from_slice::<PickerLayoutMessage>(bytes) else {
        return Vec::new();
    };
    if consumed != bytes.len() || message.message_type.as_str() != "picker-pane" {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if message.pane == 0 {
        return Vec::new();
    }
    state.picker_pane = Some(message.pane);
    state.picker_visible = message.visible;
    let focus = if message.visible {
        Some(message.pane)
    } else {
        state
            .workspace_focus
            .or_else(|| state.sidebar_visible.then_some(state.pane))
    };
    if let Some(layout) = state.layout.as_mut() {
        layout.focused_pane = focus;
    }
    publish_layout(&state, false, focus)
}

fn handle_picker_selection(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((message, consumed)) = serde_json_core::from_slice::<PickerSelection>(bytes) else {
        return Vec::new();
    };
    if consumed != bytes.len() {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if let Some(session) = message.result.value.session {
        let request = state.next_request();
        return vec![session_command(
            request,
            SessionOperation::Activate,
            Some(session),
            None,
        )];
    }
    let Some(agent) = message.result.value.agent else {
        return Vec::new();
    };
    if !state.agents.iter().any(|available| available.id == agent) {
        state.error = true;
        state.status = "The selected agent is no longer available".to_string();
        return Vec::new();
    }
    let id = format!(
        "coding-agent-ui-s{}-{}",
        state.session_id, state.next_coding_agent_ui
    );
    state.next_coding_agent_ui = state.next_coding_agent_ui.saturating_add(1);
    state.coding_agent_targets.insert(id.clone(), agent);
    vec![start_coding_agent_ui(&id)]
}

fn open_session_picker(state: &mut SwitcherState) -> Vec<ActorCommand> {
    let items = state
        .sessions
        .iter()
        .map(|session| SessionPickerItem {
            label: &session.name,
            detail: if session.windows == 1 {
                "1 window".to_string()
            } else {
                format!("{} windows", session.windows)
            },
            value: SessionPickerValueWire {
                session: session.id,
            },
        })
        .collect::<Vec<_>>();
    let request = SessionPickerRequest {
        message_type: "picker-create",
        title: "Switch session",
        placeholder: "Type to fuzzy-match sessions…",
        items,
    };
    let Ok(body) = serde_json::to_vec(&request) else {
        state.error = true;
        state.status = "Could not encode the session picker".to_string();
        redraw_or_record_error(state);
        return Vec::new();
    };
    vec![ActorCommand::SendLocalMessage(LocalActorMessage {
        actor: PICKER_ACTOR_ID.to_string(),
        body,
    })]
}

fn open_agent_picker(state: &mut SwitcherState) -> Vec<ActorCommand> {
    state.agent_picker_pending = false;
    let items = state
        .agents
        .iter()
        .map(|agent| AgentPickerItem {
            label: &agent.id,
            detail: format!("{} · {} · {}", agent.status, agent.model, agent.reasoning),
            value: AgentPickerValue { agent: &agent.id },
        })
        .collect::<Vec<_>>();
    let request = AgentPickerRequest {
        message_type: "picker-create",
        title: "Open coding agent",
        placeholder: "Type to fuzzy-match agents in this session…",
        items,
    };
    let Ok(body) = serde_json::to_vec(&request) else {
        state.error = true;
        state.status = "Could not encode the agent picker".to_string();
        return Vec::new();
    };
    vec![ActorCommand::SendLocalMessage(LocalActorMessage {
        actor: PICKER_ACTOR_ID.to_string(),
        body,
    })]
}

fn start_diff_viewer(id: &str) -> ActorCommand {
    ActorCommand::StartActor(ActorStartRequest {
        id: id.to_string(),
        path: DIFF_VIEWER_COMPONENT_FILE.to_string(),
        capabilities: vec![
            "filesystem:workspace-read".to_string(),
            "subprocess:git".to_string(),
        ],
    })
}

fn start_coding_agent_ui(id: &str) -> ActorCommand {
    ActorCommand::StartActor(ActorStartRequest {
        id: id.to_string(),
        path: CODING_AGENT_COMPONENT_FILE.to_string(),
        capabilities: Vec::new(),
    })
}

fn bind_coding_agent_ui(id: &str, session_id: u64, agent: Option<&str>) -> ActorCommand {
    ActorCommand::SendLocalMessage(LocalActorMessage {
        actor: id.to_string(),
        body: format!(
            r#"{{"kind":"bind","session_id":{session_id},"agent":"{}"}}"#,
            agent.unwrap_or_default()
        )
        .into_bytes(),
    })
}

fn subscribe_agent_registry(session_id: u64) -> ActorCommand {
    ActorCommand::SendSessionMessage(SessionActorMessage {
        session_id,
        actor: "init".to_string(),
        body: br#"{"kind":"registry-subscribe"}"#.to_vec(),
    })
}

fn actor_session(actor: &str) -> Option<u64> {
    actor
        .split("-s")
        .nth(1)
        .and_then(|suffix| suffix.split('-').next())
        .and_then(|session| session.parse().ok())
}

fn stop_terminal_command(session_id: u64, ui_actor: &str) -> Option<ActorCommand> {
    let terminal = if ui_actor == TERMINAL_UI_ID {
        "terminal-session".to_string()
    } else {
        let suffix = ui_actor.strip_prefix("terminal-ui-s")?.split_once('-')?.1;
        format!("terminal-session-{suffix}")
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "kind": "stop-terminal",
        "terminal": terminal,
    }))
    .ok()?;
    Some(ActorCommand::SendSessionMessage(SessionActorMessage {
        session_id,
        actor: "init".to_string(),
        body,
    }))
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
    let picker_pane = state.picker_pane;
    let delegated = message
        .visible_panes
        .iter()
        .copied()
        .filter(|visible| *visible != pane && Some(*visible) != picker_pane)
        .chain(message.available_windows.iter().map(|window| window.pane))
        .fold(Vec::new(), |mut panes, candidate| {
            if !panes.contains(&candidate) {
                panes.push(candidate);
            }
            panes
        });
    let needs_rest = allow_delegation && !message.rest && !delegated.is_empty();
    let has_rest = message.rest || !delegated.is_empty();
    let pane_visible = state.sidebar_visible && message.visible_panes.contains(&pane);
    let picker_visible = state.picker_visible
        && picker_pane.is_some_and(|picker| message.visible_panes.contains(&picker));
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

    let mut layout_items = Vec::new();
    if pane_visible {
        layout_items.push(format!(
            "{{\"id\":{},\"kind\":{{\"type\":\"pane\",\"pane\":{}}},\"z\":1,\"transform\":{{\"offset\":{{\"u\":{},\"v\":-1}},\"size\":{{\"u\":{},\"v\":2}}}},\"input\":\"pane\"}}",
            pane, pane, sidebar_offset, sidebar_size
        ));
    }
    if picker_visible && let Some(picker) = picker_pane {
        let height = f64::from(message.size.height.max(1));
        let picker_width = (720.0 / width).clamp(0.45, 0.88);
        let picker_height = (480.0 / height).clamp(0.40, 0.82);
        layout_items.push(format!(
            "{{\"id\":{},\"kind\":{{\"type\":\"pane\",\"pane\":{}}},\"z\":100,\"transform\":{{\"offset\":{{\"u\":{},\"v\":{}}},\"size\":{{\"u\":{},\"v\":{}}}}},\"input\":\"pane\"}}",
            picker,
            picker,
            -picker_width,
            -picker_height,
            picker_width * 2.0,
            picker_height * 2.0
        ));
    }
    let layout_items = layout_items.join(",");
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
            layout_items,
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
        return submit(state);
    }
    if y < HEADER_HEIGHT || y >= state.height - FOOTER_HEIGHT {
        return Vec::new();
    }
    let items = state.list_items();
    let mut item_y = HEADER_HEIGHT;
    let item = items.iter().skip(state.scroll).find(|item| {
        let height = item_height(item);
        let hit = y >= item_y && y < item_y + height;
        item_y += height;
        hit
    });
    let Some(item) = item.cloned() else {
        return Vec::new();
    };
    if let ListItem::Project(project) = item {
        if button != LEFT_BUTTON {
            return Vec::new();
        }
        if x >= state.width - 48.0 && project.git {
            begin_worktree(state, &project);
        } else if !state.collapsed_projects.remove(&project.id) {
            state.collapsed_projects.insert(project.id);
            state.scroll = state.scroll.min(state.list_items().len().saturating_sub(1));
        }
        state.ensure_selected_visible();
        return Vec::new();
    }
    let ListItem::Session(index) = item else {
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
            if state.mode == EditMode::Search {
                state.scroll = 0;
                select_first_filtered(state);
            }
            Vec::new()
        }
        "Delete" if state.mode == EditMode::Search && state.query.is_empty() => {
            close_selected(state)
        }
        "F2" if state.mode == EditMode::Search => {
            if let Some(session) = state.selected_session().cloned() {
                begin_rename(state, &session);
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn submit(state: &mut SwitcherState) -> Vec<ActorCommand> {
    if let EditMode::Rename(session) = &state.mode {
        let session = *session;
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
    if let EditMode::CreateWorktree(session) = &state.mode {
        let session = *session;
        let branch = state.query.trim().to_string();
        if branch.is_empty() {
            state.error = true;
            state.status = "A branch name cannot be empty".to_string();
            return Vec::new();
        }
        let request = state.next_request();
        return vec![session_command(
            request,
            SessionOperation::CreateWorktree,
            Some(session),
            Some(branch),
        )];
    }
    let visible_sessions = state
        .list_items()
        .into_iter()
        .filter_map(|item| match item {
            ListItem::Project(_) => None,
            ListItem::Session(index) => Some(state.sessions[index].id),
        })
        .collect::<Vec<_>>();
    if let Some(selected) = state.selected
        && visible_sessions.contains(&selected)
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

fn begin_worktree(state: &mut SwitcherState, project: &ProjectGroup) {
    state.mode = EditMode::CreateWorktree(project.source_session);
    state.query.clear();
    state.status = format!("New branch in {}: Enter to create", project.name);
    state.error = false;
}

fn move_selection(state: &mut SwitcherState, delta: isize) {
    let visible = state
        .list_items()
        .into_iter()
        .filter_map(|item| match item {
            ListItem::Project(_) => None,
            ListItem::Session(index) => Some(index),
        })
        .collect::<Vec<_>>();
    if visible.is_empty() {
        state.selected = None;
        return;
    }
    let current = state
        .selected
        .and_then(|selected| {
            visible
                .iter()
                .position(|index| state.sessions[*index].id == selected)
        })
        .unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(visible.len() - 1)
    };
    state.selected = Some(state.sessions[visible[next]].id);
    state.ensure_selected_visible();
}

fn select_first_filtered(state: &mut SwitcherState) {
    state.selected = state.list_items().into_iter().find_map(|item| match item {
        ListItem::Project(_) => None,
        ListItem::Session(index) => Some(state.sessions[index].id),
    });
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

    let input_color = if state.error {
        RED
    } else if state.mode == EditMode::Search {
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
        match &state.mode {
            EditMode::Search => "Search or create…".to_string(),
            EditMode::Rename(_) => "New session name…".to_string(),
            EditMode::CreateWorktree(_) => "Branch name…".to_string(),
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
        } else if state.mode == EditMode::Search {
            "+ New"
        } else {
            "Create"
        },
        14.0,
        PANEL_DARK,
        state.width - 68.0,
        59.0,
        48.0,
    )?;

    let items = state.list_items();
    let visible = state.visible_item_count(&items, state.scroll);
    packet.push(Command::Save);
    packet.push(Command::ClipRect(Rect::new(
        0.0,
        HEADER_HEIGHT,
        state.width,
        state.height - HEADER_HEIGHT - FOOTER_HEIGHT,
    )));
    let mut y = HEADER_HEIGHT;
    for item in items.iter().skip(state.scroll).take(visible) {
        match item {
            ListItem::Project(project) => {
                packet.push(Command::FillRect {
                    rect: Rect::new(0.0, y, state.width, PROJECT_ROW_HEIGHT),
                    color: PANEL_DARK,
                });
                let collapsed = state.collapsed_projects.contains(&project.id);
                add_text(
                    state,
                    &mut packet,
                    &mut resources,
                    if collapsed { "▸" } else { "▾" },
                    14.0,
                    MUTED,
                    SIDE_MARGIN,
                    y + 8.0,
                    14.0,
                )?;
                add_text(
                    state,
                    &mut packet,
                    &mut resources,
                    &truncate(&project.name, 22),
                    14.0,
                    FG,
                    SIDE_MARGIN + 18.0,
                    y + 8.0,
                    state.width - 126.0,
                )?;
                add_text(
                    state,
                    &mut packet,
                    &mut resources,
                    &project.sessions.len().to_string(),
                    12.0,
                    MUTED,
                    state.width - 76.0,
                    y + 9.0,
                    24.0,
                )?;
                if project.git {
                    add_text(
                        state,
                        &mut packet,
                        &mut resources,
                        "+",
                        20.0,
                        YELLOW,
                        state.width - 36.0,
                        y + 4.0,
                        20.0,
                    )?;
                }
            }
            ListItem::Session(index) => {
                let session = state.sessions[*index].clone();
                let selected = state.selected == Some(session.id);
                if session.active || selected {
                    packet.push(Command::FillRoundedRect {
                        rect: Rect::new(
                            SIDE_MARGIN + 12.0,
                            y + 3.0,
                            state.width - 40.0,
                            ROW_HEIGHT - 6.0,
                        ),
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
                        rect: Rect::new(
                            SIDE_MARGIN + 12.0,
                            y + 3.0,
                            state.width - 40.0,
                            ROW_HEIGHT - 6.0,
                        ),
                        radius: 6.0,
                        color: YELLOW,
                        width: 1.5,
                    });
                }
                add_text(
                    state,
                    &mut packet,
                    &mut resources,
                    &truncate(&session.name, 24),
                    16.0,
                    FG,
                    SIDE_MARGIN + 22.0,
                    y + 8.0,
                    state.width - 158.0,
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
        }
        y += item_height(item);
    }
    packet.push(Command::Restore);

    if items.len() > visible {
        let track_height = state.height - HEADER_HEIGHT - FOOTER_HEIGHT - 12.0;
        let thumb_height = (track_height * visible as f32 / items.len() as f32).max(24.0);
        let max_scroll = items.len().saturating_sub(visible).max(1);
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
    } else if items.is_empty() {
        "No match — Enter creates it".to_string()
    } else {
        "Enter open  ·  F2 rename  ·  Delete close".to_string()
    };
    add_text(
        state,
        &mut packet,
        &mut resources,
        &if state.error {
            truncate(&footer, 150)
        } else {
            truncate(&footer, 48)
        },
        12.0,
        if state.error { RED } else { MUTED },
        SIDE_MARGIN,
        state.height - FOOTER_HEIGHT + 8.0,
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

const fn item_height(item: &ListItem) -> f32 {
    match item {
        ListItem::Project(_) => PROJECT_ROW_HEIGHT,
        ListItem::Session(_) => ROW_HEIGHT,
    }
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
            font_intent: host::FontIntent::Ui,
            font_weight: host::FontWeight::Normal,
            font_style: host::FontStyle::Normal,
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
