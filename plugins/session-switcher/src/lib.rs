use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use brood_graphics::{Color, Command, Packet, Rect, theme::ashen};
use brood_plugin_sdk::{
    bindings,
    exports::brood::plugin::actor::{
        ActorStartRequest, ChildEvent, Command as ActorCommand, DaemonActorMessage, Event, Guest,
        LifecycleEvent, LocalActorMessage, PresentationEvent, SessionActorMessage, SessionCommand,
        SessionCommandCompleted, SessionEvent, SessionOperation, SessionSnapshot,
    },
    graphics::{drop_resources, layout_ui_text, submit_packet},
    input::{BUTTON_PRIMARY as LEFT_BUTTON, BUTTON_SECONDARY as RIGHT_BUTTON},
    text::truncate_to_width,
};
use serde::{Deserialize, Serialize};

const INIT_ACTOR_ID: &str = "brood.client-init";
const CHILD_ACTOR_ID: &str = "session-switcher";
const PICKER_ACTOR_ID: &str = "picker";
const INBOX_ACTOR_ID: &str = "inbox";
const COMPONENT_FILE: &str = "brood_session_switcher_plugin.component.wasm";
const PICKER_COMPONENT_FILE: &str = "brood_picker_plugin.component.wasm";
const INBOX_COMPONENT_FILE: &str = "brood_inbox_plugin.component.wasm";
const CODING_AGENT_COMPONENT_FILE: &str = "brood_coding_agent_plugin.component.wasm";
const NEOVIM_CLIENT_COMPONENT_FILE: &str = "brood_neovim_client_plugin.component.wasm";
const TERMINAL_UI_COMPONENT_FILE: &str = "brood_terminal_ui_plugin.component.wasm";
const TERMINAL_UI_ID: &str = "terminal-ui";
const NEOVIM_UI_ID_PREFIX: &str = "neovim-ui-b";
const NEOVIM_FILES_ID: &str = "neovim-files";
const CODING_AGENT_UI_ID: &str = "coding-agent-ui";
const HEADER_HEIGHT: f32 = 52.0;
const FOOTER_HEIGHT: f32 = 28.0;
const ROW_HEIGHT: f32 = 30.0;
const PROJECT_ROW_HEIGHT: f32 = 34.0;
const SIDE_MARGIN: f32 = 14.0;
const SIDEBAR_WIDTH: f64 = 320.0;
const INBOX_WIDTH: f64 = 320.0;

const BG: Color = ashen::BACKGROUND;
const SURFACE: Color = ashen::SURFACE;
const SURFACE_RAISED: Color = ashen::SURFACE_RAISED;
const HOVER: Color = ashen::INTERACTIVE_HOVER;
const PRESSED: Color = ashen::INTERACTIVE_PRESSED;
const ACTIVE: Color = ashen::CURSOR_LINE;
const BORDER: Color = ashen::BORDER;
const FG: Color = ashen::TEXT;
const TEXT: Color = ashen::TEXT_STRONG;
const MUTED: Color = ashen::MUTED;
const YELLOW: Color = ashen::ACCENT;
const RED: Color = ashen::ERROR;

#[derive(Clone)]
struct ViewSession {
    backend: u64,
    id: u64,
    name: String,
    active: bool,
    windows: u64,
    working_directory: Option<String>,
    project_id: String,
    project_name: String,
    branch: Option<String>,
    worktree: bool,
    git: bool,
}

#[derive(Clone, Eq, PartialEq)]
enum EditMode {
    Search,
    Rename(u64, u64),
    CreateWorktree(u64, u64),
    ConnectRemote,
}

#[derive(Clone, Deserialize)]
struct BackendSessionsMessage {
    kind: serde_json_core::heapless::String<32>,
    #[serde(default)]
    can_create_local: bool,
    #[serde(default)]
    can_connect_remote: bool,
    backends: Vec<BackendView>,
}

#[derive(Deserialize)]
struct BackendConnectedMessage {
    kind: serde_json_core::heapless::String<32>,
    backend: u64,
}

#[derive(Deserialize)]
struct BackendConnectionFailedMessage {
    kind: serde_json_core::heapless::String<32>,
    message: String,
}

#[derive(Clone, Deserialize)]
struct BackendView {
    backend: u64,
    label: String,
    sessions: Vec<BackendSession>,
}

#[derive(Clone, Deserialize)]
struct BackendSession {
    id: u64,
    name: String,
    active: bool,
    windows: u64,
    working_directory: Option<String>,
    project_id: String,
    project_name: String,
    branch: Option<String>,
    worktree: bool,
    git: bool,
}

#[derive(Clone)]
struct ProjectGroup {
    backend: u64,
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum SwitcherTarget {
    NewSession,
    ProjectToggle(u64, String),
    ProjectWorktree(u64, String),
    Session(u64, u64),
    SessionClose(u64, u64),
}

struct SwitcherState {
    is_root: bool,
    can_create_local: bool,
    can_connect_remote: bool,
    pane: u64,
    backend_id: u64,
    session_id: u64,
    sidebar_visible: bool,
    width: f32,
    height: f32,
    sessions: Vec<ViewSession>,
    collapsed_projects: BTreeSet<String>,
    selected: Option<(u64, u64)>,
    query: String,
    mode: EditMode,
    scroll: usize,
    request: u64,
    pending: Option<u64>,
    pending_directory_backend: Option<u64>,
    status: String,
    error: bool,
    next_coding_agent_ui: u64,
    coding_agent_uis: Vec<String>,
    coding_agent_targets: BTreeMap<String, String>,
    agents: Vec<WireAgent>,
    agent_picker_pending: bool,
    terminal_uis: BTreeMap<String, (u64, u64)>,
    neovim_uis_starting: BTreeMap<(u64, u64), String>,
    neovim_uis: BTreeMap<(u64, u64), String>,
    text_resources: Vec<u64>,
    layout: Option<LayoutContext>,
    workspace_focus: Option<u64>,
    picker_pane: Option<u64>,
    picker_visible: bool,
    inbox_pane: Option<u64>,
    hovered: Option<SwitcherTarget>,
    pressed: Option<SwitcherTarget>,
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
    backend: u64,
    pane: u64,
    session: u64,
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

#[derive(Default, Deserialize)]
#[serde(default)]
struct EditorMessage {
    kind: String,
    action: String,
    source: String,
    backend_id: u64,
    session_id: u64,
    editor: Option<String>,
    command: String,
    message: String,
    path: String,
    prompt: String,
    agent: String,
    name: String,
    target: String,
    body: serde_json::Value,
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
struct InboxReadyMessage {
    kind: serde_json_core::heapless::String<32>,
    pane: u64,
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
    backend: Option<u64>,
    #[serde(default)]
    action: Option<serde_json_core::heapless::String<32>>,
    #[serde(default)]
    directory: Option<String>,
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
struct WorktreeSessionRequest<'a> {
    kind: &'static str,
    request: u64,
    source_session: u64,
    branch: &'a str,
}

#[derive(Serialize)]
struct DirectoryListRequest {
    kind: &'static str,
    request: u64,
    roots: Vec<String>,
}

#[derive(Deserialize)]
struct DirectoryListCompletion {
    #[serde(rename = "type")]
    message_type: String,
    request: u64,
    success: bool,
    #[serde(default)]
    directories: Vec<String>,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct WorktreeSessionCompletion {
    #[serde(rename = "type")]
    message_type: serde_json_core::heapless::String<64>,
    request: u64,
    success: bool,
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    message: serde_json_core::heapless::String<256>,
}

#[derive(Serialize)]
struct SessionPickerRequest {
    #[serde(rename = "type")]
    message_type: &'static str,
    title: &'static str,
    placeholder: &'static str,
    items: Vec<SessionPickerItem>,
}

#[derive(Serialize)]
struct SessionPickerItem {
    label: String,
    detail: String,
    value: SessionPickerValueWire,
}

#[derive(Serialize)]
struct SessionPickerValueWire {
    #[serde(default)]
    backend: Option<u64>,
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    directory: Option<String>,
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
            is_root: false,
            can_create_local: false,
            can_connect_remote: false,
            pane: 0,
            backend_id: 0,
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
            pending_directory_backend: None,
            status: String::new(),
            error: false,
            next_coding_agent_ui: 2,
            coding_agent_uis: Vec::new(),
            coding_agent_targets: BTreeMap::new(),
            agents: Vec::new(),
            agent_picker_pending: false,
            terminal_uis: BTreeMap::new(),
            neovim_uis_starting: BTreeMap::new(),
            neovim_uis: BTreeMap::new(),
            text_resources: Vec::new(),
            layout: None,
            workspace_focus: None,
            picker_pane: None,
            picker_visible: false,
            inbox_pane: None,
            hovered: None,
            pressed: None,
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
                .find(|group| group.backend == session.backend && group.id == session.project_id)
            {
                group.sessions.push(index);
                let source_is_worktree = self
                    .sessions
                    .iter()
                    .find(|candidate| {
                        candidate.backend == group.backend && candidate.id == group.source_session
                    })
                    .is_some_and(|candidate| candidate.worktree);
                if source_is_worktree && !session.worktree {
                    group.source_session = session.id;
                }
            } else {
                groups.push(ProjectGroup {
                    backend: session.backend,
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
            let project_key = format!("{}:{}", group.backend, group.id);
            let expanded = query.is_some() || !self.collapsed_projects.contains(&project_key);
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
        if self.selected.is_some_and(|selected| {
            self.sessions
                .iter()
                .any(|session| (session.backend, session.id) == selected)
        }) {
            return;
        }
        self.selected = self
            .sessions
            .iter()
            .find(|session| session.active)
            .or_else(|| self.sessions.first())
            .map(|session| (session.backend, session.id));
    }

    fn ensure_selected_visible(&mut self) {
        let items = self.list_items();
        let Some(position) = self.selected.and_then(|selected| {
            items
                .iter()
                .position(|item| matches!(item, ListItem::Session(index) if (self.sessions[*index].backend, self.sessions[*index].id) == selected))
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
        self.sessions
            .iter()
            .find(|session| (session.backend, session.id) == selected)
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
                        state.is_root = true;
                        state.backend_id = context.backend_id.unwrap_or_default();
                        state.session_id = context.session_id.unwrap_or_default();
                        if let (Some(backend_id), Some(session_id)) =
                            (context.backend_id, context.session_id)
                        {
                            state
                                .terminal_uis
                                .insert(TERMINAL_UI_ID.to_string(), (backend_id, session_id));
                        }
                    }
                    let mut commands = vec![
                        ActorCommand::StartActor(ActorStartRequest {
                            id: CHILD_ACTOR_ID.to_string(),
                            path: COMPONENT_FILE.to_string(),
                            capabilities: Vec::new(),
                            backend_id: None,
                            session_id: None,
                        }),
                        ActorCommand::StartActor(ActorStartRequest {
                            id: PICKER_ACTOR_ID.to_string(),
                            path: PICKER_COMPONENT_FILE.to_string(),
                            capabilities: Vec::new(),
                            backend_id: None,
                            session_id: None,
                        }),
                        ActorCommand::StartActor(ActorStartRequest {
                            id: INBOX_ACTOR_ID.to_string(),
                            path: INBOX_COMPONENT_FILE.to_string(),
                            capabilities: Vec::new(),
                            backend_id: None,
                            session_id: None,
                        }),
                    ];
                    if let (Some(backend_id), Some(session_id)) =
                        (context.backend_id, context.session_id)
                    {
                        commands.push(ActorCommand::StartActor(ActorStartRequest {
                            id: TERMINAL_UI_ID.to_string(),
                            path: TERMINAL_UI_COMPONENT_FILE.to_string(),
                            capabilities: Vec::new(),
                            backend_id: Some(backend_id),
                            session_id: Some(session_id),
                        }));
                        commands.push(subscribe_agent_registry(backend_id, session_id));
                        commands.push(query_editor_status(backend_id, session_id));
                    }
                    commands.push(ActorCommand::LifecycleComplete(Ok(())));
                    commands
                } else {
                    vec![ActorCommand::LifecycleComplete(Ok(()))]
                }
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Child(ChildEvent::Started(started))
                if started.id == TERMINAL_UI_ID || started.id.starts_with("terminal-ui-b") =>
            {
                if let Ok(mut state) = STATE.lock() {
                    let realm =
                        actor_realm(&started.id).unwrap_or((state.backend_id, state.session_id));
                    state.terminal_uis.insert(started.id, realm);
                }
                Vec::new()
            }
            Event::Child(ChildEvent::Started(started))
                if neovim_client_realm(&started.id).is_some() =>
            {
                if let Some(realm) = neovim_client_realm(&started.id)
                    && let Ok(mut state) = STATE.lock()
                    && state
                        .neovim_uis_starting
                        .get(&realm)
                        .is_some_and(|actor| actor == &started.id)
                {
                    state.neovim_uis_starting.remove(&realm);
                    state.neovim_uis.insert(realm, started.id);
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
                let (backend, session) =
                    actor_realm(&started.id).unwrap_or((state.backend_id, state.session_id));
                vec![bind_coding_agent_ui(
                    &started.id,
                    backend,
                    session,
                    target.as_deref(),
                )]
            }
            Event::Child(ChildEvent::Stopped(id)) => {
                let Ok(mut state) = STATE.lock() else {
                    return Vec::new();
                };
                if id == INBOX_ACTOR_ID {
                    state.inbox_pane = None;
                    state.error = true;
                    state.status = "The global inbox stopped".to_string();
                    return Vec::new();
                }
                state.coding_agent_uis.retain(|actor| actor != &id);
                state.coding_agent_targets.remove(&id);
                let editor_lifecycle =
                    remove_neovim_ui(&mut state, &id).map(|(backend_id, session_id)| {
                        editor_client_lifecycle(
                            "editor-client-stopped",
                            &id,
                            backend_id,
                            session_id,
                            "",
                        )
                    });
                let terminal_stop =
                    state
                        .terminal_uis
                        .remove(&id)
                        .map_or_else(Vec::new, |(backend, session)| {
                            state
                                .sessions
                                .iter()
                                .any(|candidate| {
                                    candidate.backend == backend && candidate.id == session
                                })
                                .then(|| stop_terminal_command(backend, session, &id))
                                .flatten()
                                .into_iter()
                                .collect()
                        });
                editor_lifecycle.into_iter().chain(terminal_stop).collect()
            }
            Event::Child(ChildEvent::Failed(failed)) => {
                let Ok(mut state) = STATE.lock() else {
                    return Vec::new();
                };
                if failed.id == INBOX_ACTOR_ID {
                    state.inbox_pane = None;
                    state.error = true;
                    state.status = format!("The global inbox failed: {}", failed.message);
                    return Vec::new();
                }
                state.coding_agent_uis.retain(|actor| actor != &failed.id);
                state.coding_agent_targets.remove(&failed.id);
                let editor_failure =
                    remove_neovim_ui_any(&mut state, &failed.id).map(|(backend_id, session_id)| {
                        editor_client_lifecycle(
                            "editor-client-failed",
                            &failed.id,
                            backend_id,
                            session_id,
                            &failed.message,
                        )
                    });
                let stop = state
                    .terminal_uis
                    .remove(&failed.id)
                    .and_then(|(backend, session)| {
                        stop_terminal_command(backend, session, &failed.id)
                    });
                state.error = true;
                state.status = format!("Plugin pane failed: {}", failed.message);
                editor_failure.into_iter().chain(stop).collect()
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
            Event::Mailbox(message) if message.sender == INBOX_ACTOR_ID => {
                handle_inbox_message(&message.body)
            }
            Event::Mailbox(message) if message.sender.starts_with(NEOVIM_UI_ID_PREFIX) => {
                handle_editor_message(&message.sender, EditorMessage::from_bytes(&message.body))
            }
            Event::Mailbox(message)
                if message.sender.starts_with("session:") || message.sender == "client" =>
            {
                handle_session_message(&message.sender, &message.body)
            }
            Event::Mailbox(message) if message.sender.starts_with("daemon:") => {
                handle_daemon_message(&message.sender, &message.body)
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

fn handle_inbox_message(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((message, consumed)) = serde_json_core::from_slice::<InboxReadyMessage>(bytes) else {
        return Vec::new();
    };
    if consumed != bytes.len() || message.kind.as_str() != "inbox-ready" || message.pane == 0 {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    state.inbox_pane = Some(message.pane);
    publish_layout(&state, true, state.workspace_focus)
}

fn handle_session_event(event: SessionEvent) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let commands = match event {
        SessionEvent::Snapshot(snapshot) => {
            let backend = snapshot.backend_id;
            let active = snapshot.active;
            let previous_realm = (state.backend_id, state.session_id);
            apply_snapshot(&mut state, backend, snapshot);
            if active != 0 && (state.backend_id == 0 || state.backend_id == backend) {
                state.backend_id = backend;
                state.session_id = active;
            } else if active == 0 && state.backend_id == backend {
                let fallback = state
                    .sessions
                    .iter()
                    .find(|session| session.active)
                    .or_else(|| state.sessions.first())
                    .map(|session| (session.backend, session.id))
                    .unwrap_or_default();
                state.backend_id = fallback.0;
                state.session_id = fallback.1;
            }
            let mut commands = if state.is_root {
                ensure_default_terminal_ui(&mut state, backend, active)
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            if state.is_root && backend != 0 && active != 0 {
                commands.push(query_editor_status(backend, active));
            }
            if previous_realm != (state.backend_id, state.session_id) {
                state.agents.clear();
                if state.backend_id != 0 && state.session_id != 0 {
                    commands.push(subscribe_agent_registry(state.backend_id, state.session_id));
                }
            }
            commands
        }
        SessionEvent::CommandCompleted(completion) => {
            if completion.success
                && matches!(
                    completion.operation,
                    SessionOperation::Activate | SessionOperation::Create
                )
                && let Some(session) = completion.session
            {
                state.backend_id = completion.backend_id;
                state.session_id = session;
            }
            apply_completion(&mut state, completion);
            Vec::new()
        }
    };
    redraw_or_record_error(&mut state);
    commands
}

fn apply_snapshot(state: &mut SwitcherState, backend: u64, snapshot: SessionSnapshot) {
    state.sessions.retain(|session| session.backend != backend);
    state
        .sessions
        .extend(snapshot.sessions.into_iter().map(|session| ViewSession {
            backend,
            id: session.id,
            name: session.name,
            active: session.active,
            windows: session.windows,
            working_directory: session.working_directory,
            project_id: session.project_id,
            project_name: session.project_name,
            branch: session.branch,
            worktree: session.worktree,
            git: session.git,
        }));
    state.sessions.sort_by(|left, right| {
        left.project_name
            .to_lowercase()
            .cmp(&right.project_name.to_lowercase())
            .then_with(|| left.backend.cmp(&right.backend))
            .then_with(|| left.id.cmp(&right.id))
    });
    let projects = state
        .sessions
        .iter()
        .map(|session| format!("{}:{}", session.backend, session.project_id))
        .collect::<BTreeSet<_>>();
    state
        .collapsed_projects
        .retain(|project| projects.contains(project));
    state.ensure_selection();
    state.ensure_selected_visible();
}

fn apply_backend_sessions(state: &mut SwitcherState, message: BackendSessionsMessage) {
    if message.kind.as_str() != "backend-sessions" {
        return;
    }
    state.can_create_local = message.can_create_local;
    state.can_connect_remote = message.can_connect_remote;
    state.sessions = message
        .backends
        .into_iter()
        .flat_map(|backend| {
            backend
                .sessions
                .into_iter()
                .map(move |session| ViewSession {
                    backend: backend.backend,
                    id: session.id,
                    name: session.name,
                    active: session.active,
                    windows: session.windows,
                    working_directory: session.working_directory,
                    project_id: session.project_id,
                    project_name: if backend.label.is_empty() {
                        session.project_name
                    } else {
                        format!("{} · {}", backend.label, session.project_name)
                    },
                    branch: session.branch,
                    worktree: session.worktree,
                    git: session.git,
                })
        })
        .collect();
    state
        .sessions
        .sort_by_key(|session| (session.backend, session.id));
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
            state.selected = state
                .sessions
                .iter()
                .find(|candidate| {
                    candidate.backend == completion.backend_id && candidate.id == session
                })
                .map(|candidate| (candidate.backend, candidate.id));
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

fn handle_daemon_message(sender: &str, bytes: &[u8]) -> Vec<ActorCommand> {
    if let Ok(completion) = serde_json::from_slice::<DirectoryListCompletion>(bytes)
        && completion.message_type == "directory-list"
    {
        let Ok(mut state) = STATE.lock() else {
            return Vec::new();
        };
        if state.pending != Some(completion.request) {
            return Vec::new();
        }
        state.pending = None;
        if !completion.success {
            state.error = true;
            state.status = if completion.message.is_empty() {
                "The backend could not list its directories".to_string()
            } else {
                completion.message
            };
            redraw_or_record_error(&mut state);
            return Vec::new();
        }
        let backend = backend_from_sender(sender).unwrap_or(state.backend_id);
        state.error = false;
        state.status.clear();
        return show_directory_picker(&mut state, backend, completion.directories);
    }
    let Ok((completion, consumed)) =
        serde_json_core::from_slice::<WorktreeSessionCompletion>(bytes)
    else {
        return Vec::new();
    };
    if consumed != bytes.len() || completion.message_type.as_str() != "worktree-session-completed" {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if state.pending == Some(completion.request) {
        state.pending = None;
    }
    state.error = !completion.success;
    if completion.success {
        let backend = backend_from_sender(sender).unwrap_or(state.backend_id);
        if let Some(session) = completion.session {
            state.selected = state
                .sessions
                .iter()
                .find(|candidate| candidate.backend == backend && candidate.id == session)
                .map(|candidate| (candidate.backend, candidate.id));
        }
        state.status = "Created worktree session".to_string();
        state.query.clear();
        state.mode = EditMode::Search;
    } else {
        state.status = completion.message.as_str().to_string();
    }
    redraw_or_record_error(&mut state);
    Vec::new()
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
            vec![ActorCommand::SendHostMessage(
                br#"{"type":"claim-root-layout"}"#.to_vec(),
            )]
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            state.width = resized.width.max(240.0);
            state.height = resized.height.max(240.0);
            state.hovered = None;
            state.pressed = None;
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
            if !focused.focused {
                state.hovered = None;
                state.pressed = None;
                redraw_or_record_error(&mut state);
            }
            Vec::new()
        }
        PresentationEvent::PointerMoved(moved) if moved.pane == state.pane => {
            let hovered = switcher_target_at(&state, moved.x, moved.y);
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
            let target = switcher_target_at(&state, button.x, button.y);
            let commands = if button.pressed {
                state.hovered.clone_from(&target);
                state.pressed = target;
                Vec::new()
            } else {
                let pressed = state.pressed.take();
                state.hovered.clone_from(&target);
                match (pressed, target) {
                    (Some(pressed), Some(released)) if pressed == released => {
                        activate_switcher_target(&mut state, pressed)
                    }
                    _ => Vec::new(),
                }
            };
            redraw_or_record_error(&mut state);
            commands
        }
        PresentationEvent::PointerButton(button)
            if button.pane == state.pane && button.pressed && button.button == RIGHT_BUTTON =>
        {
            let commands = handle_secondary_click(&mut state, button.x, button.y);
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
    let focused_realm = message.focused_pane.and_then(|focused| {
        message
            .available_windows
            .iter()
            .find(|window| window.pane == focused)
            .map(|window| (window.backend, window.session))
            .filter(|(backend, session)| *backend != 0 && *session != 0)
    });
    let realm_changed =
        focused_realm.is_some_and(|realm| realm != (state.backend_id, state.session_id));
    if let Some((backend, session)) = focused_realm {
        state.backend_id = backend;
        state.session_id = session;
        if realm_changed {
            state.agents.clear();
        }
    }
    state.layout = Some(LayoutContext {
        revision: message.revision,
        lease: message.lease,
        visible_panes: message.visible_panes,
        focused_pane: message.focused_pane,
        size: message.size,
        rest: message.rest,
    });
    let mut commands = publish_layout(&state, true, message.focused_pane);
    if realm_changed {
        commands.push(subscribe_agent_registry(state.backend_id, state.session_id));
    }
    commands
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
            return vec![subscribe_agent_registry(state.backend_id, state.session_id)];
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
        let backend_id = state.backend_id;
        let session_id = state.session_id;
        return show_diff(&mut state, backend_id, session_id);
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
            backend_id: state.backend_id,
            session_id: state.session_id,
            actor: "init".to_string(),
            body: br#"{"kind":"create-terminal"}"#.to_vec(),
        })];
    }
    if let Ok((message, consumed)) = serde_json_core::from_slice::<CodingAgentActionMessage>(bytes)
        && consumed == bytes.len()
        && message.kind.as_str() == "editor-action"
        && message.action.as_str() == "open"
    {
        let Ok(state) = STATE.lock() else {
            return Vec::new();
        };
        if state.backend_id == 0 || state.session_id == 0 {
            return Vec::new();
        }
        return vec![request_editor(
            state.backend_id,
            state.session_id,
            "create-editor",
        )];
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
        "new" => return open_creation_backend_picker(&mut state),
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

fn handle_session_message(sender: &str, bytes: &[u8]) -> Vec<ActorCommand> {
    if let Ok(message) = serde_json::from_slice::<EditorMessage>(bytes)
        && (message.kind == "editor-started"
            || message.kind == "editor-present"
            || message.kind == "editor-stop"
            || message.kind == "editor-failed"
            || message.kind == "editor-opened"
            || message.kind == "editor-open-failed"
            || message.kind == "editor-action")
    {
        return handle_editor_message(sender, message);
    }
    if let Ok((message, consumed)) =
        serde_json_core::from_slice::<BackendConnectionFailedMessage>(bytes)
        && consumed == bytes.len()
        && message.kind.as_str() == "backend-connection-failed"
    {
        if let Ok(mut state) = STATE.lock() {
            state.mode = EditMode::Search;
            state.pending_directory_backend = None;
            state.error = true;
            state.status = message.message;
            redraw_or_record_error(&mut state);
        }
        return Vec::new();
    }
    if let Ok((message, consumed)) = serde_json_core::from_slice::<BackendConnectedMessage>(bytes)
        && consumed == bytes.len()
        && message.kind.as_str() == "backend-connected"
    {
        if let Ok(mut state) = STATE.lock() {
            state.pending_directory_backend = Some(message.backend);
            state.status = format!("Waiting for backend {} inventory…", message.backend);
            redraw_or_record_error(&mut state);
        }
        return Vec::new();
    }
    if let Ok((message, consumed)) = serde_json_core::from_slice::<BackendSessionsMessage>(bytes)
        && consumed == bytes.len()
        && message.kind.as_str() == "backend-sessions"
    {
        if let Ok(mut state) = STATE.lock() {
            apply_backend_sessions(&mut state, message);
            if let Some(backend) = state.pending_directory_backend
                && state
                    .sessions
                    .iter()
                    .any(|session| session.backend == backend)
            {
                return open_directory_picker(&mut state, backend);
            }
            redraw_or_record_error(&mut state);
        }
        return Vec::new();
    }
    if let Ok((message, consumed)) = serde_json_core::from_slice::<AgentRegistryMessage>(bytes)
        && consumed == bytes.len()
        && message.kind.as_str() == "registry"
    {
        let Ok(mut state) = STATE.lock() else {
            return Vec::new();
        };
        if message.session_id != state.session_id
            || backend_from_sender(sender) != Some(state.backend_id)
        {
            return Vec::new();
        }
        state.agents = message.agents;
        if state.agent_picker_pending && !state.agents.is_empty() {
            return open_agent_picker(&mut state);
        }
        return Vec::new();
    }
    handle_terminal_lifecycle(sender, bytes)
}

fn handle_terminal_lifecycle(sender: &str, bytes: &[u8]) -> Vec<ActorCommand> {
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
    let backend_id = backend_from_sender(sender).unwrap_or(state.backend_id);
    let id = format!("terminal-ui-b{backend_id}-s{session_id}-{suffix}");
    state
        .terminal_uis
        .insert(id.clone(), (backend_id, session_id));
    vec![ActorCommand::StartActor(ActorStartRequest {
        id,
        path: TERMINAL_UI_COMPONENT_FILE.to_string(),
        capabilities: Vec::new(),
        backend_id: Some(backend_id),
        session_id: Some(session_id),
    })]
}

fn backend_from_sender(sender: &str) -> Option<u64> {
    let mut parts = sender.split(':');
    match parts.next()? {
        "session" | "daemon" => parts.next()?.parse().ok(),
        _ => None,
    }
}

fn handle_editor_message(sender: &str, message: EditorMessage) -> Vec<ActorCommand> {
    let local_realm = {
        let Ok(state) = STATE.lock() else {
            return Vec::new();
        };
        authenticated_neovim_client(&state, sender)
    };
    let local_client = local_realm.is_some();
    let Some((backend_id, session_id)) = (if let Some((backend, session)) = local_realm {
        (message.source == "neovim-client"
            && message.backend_id == backend
            && message.session_id == session)
            .then_some((backend, session))
    } else {
        backend_from_sender(sender).zip((message.session_id != 0).then_some(message.session_id))
    }) else {
        return Vec::new();
    };
    match message.kind.as_str() {
        "editor-started" | "editor-present"
            if sender_actor_is(sender, "init")
                && message.editor.as_deref() == Some(NEOVIM_FILES_ID) =>
        {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            if let Some(actor) = state.neovim_uis.get(&(backend_id, session_id)) {
                return (message.kind == "editor-started")
                    .then(|| {
                        ActorCommand::SendLocalMessage(LocalActorMessage {
                            actor: actor.clone(),
                            body: br#"{"kind":"focus"}"#.to_vec(),
                        })
                    })
                    .into_iter()
                    .collect();
            }
            if state
                .neovim_uis_starting
                .contains_key(&(backend_id, session_id))
            {
                return Vec::new();
            }
            let id = neovim_ui_id(backend_id, session_id);
            state
                .neovim_uis_starting
                .insert((backend_id, session_id), id.clone());
            vec![start_neovim_client(id, backend_id, session_id)]
        }
        "editor-stop" if sender_actor_is(sender, "init") => {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            remove_neovim_ui_realm(&mut state, (backend_id, session_id))
                .map(ActorCommand::StopActor)
                .into_iter()
                .collect()
        }
        "editor-failed" if sender_actor_is(sender, "init") => {
            let mut stop = None;
            if let Ok(mut state) = STATE.lock() {
                state.error = true;
                state.status = if message.message.is_empty() {
                    "Neovim editor failed to start".to_string()
                } else {
                    format!("Neovim editor failed to start: {}", message.message)
                };
                stop = remove_neovim_ui_realm(&mut state, (backend_id, session_id))
                    .map(ActorCommand::StopActor);
                redraw_or_record_error(&mut state);
            }
            stop.into_iter().collect()
        }
        "editor-open-failed" if local_client => {
            if let Ok(mut state) = STATE.lock() {
                state.error = true;
                state.status = if message.path.is_empty() {
                    format!("Neovim open failed: {}", message.message)
                } else {
                    format!(
                        "Neovim could not open {}: {}",
                        message.path, message.message
                    )
                };
                redraw_or_record_error(&mut state);
            }
            Vec::new()
        }
        "editor-opened" if local_client => {
            if let Ok(mut state) = STATE.lock() {
                state.error = false;
                state.status = if message.path.is_empty() {
                    "Neovim opened a remote file".to_string()
                } else {
                    format!("Neovim opened {}", message.path)
                };
                redraw_or_record_error(&mut state);
            }
            Vec::new()
        }
        "editor-action"
            if local_client
                && message.command.len() <= 8 * 1024
                && message.prompt.len() <= 8 * 1024
                && !message.command.contains('\0')
                && !message.prompt.contains('\0') =>
        {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            match message.action.as_str() {
                "diff" => show_diff(&mut state, backend_id, session_id),
                "terminal" => {
                    let Ok(body) = serde_json::to_vec(&serde_json::json!({
                        "kind": "create-terminal",
                        "command": message.command,
                    })) else {
                        return Vec::new();
                    };
                    vec![ActorCommand::SendSessionMessage(SessionActorMessage {
                        backend_id,
                        session_id,
                        actor: "init".to_string(),
                        body,
                    })]
                }
                "agent" if !message.prompt.trim().is_empty() => start_editor_agent_work(
                    &mut state,
                    backend_id,
                    session_id,
                    &message.agent,
                    &message.prompt,
                ),
                "generic" => {
                    let target = if message.target.is_empty() {
                        &message.name
                    } else {
                        &message.target
                    };
                    if !valid_editor_actor_target(target) {
                        return Vec::new();
                    }
                    let body = if let Some(body) = message.body.as_str() {
                        body.as_bytes().to_vec()
                    } else {
                        let Ok(body) = serde_json::to_vec(&message.body) else {
                            return Vec::new();
                        };
                        body
                    };
                    if body.len() > 8 * 1024 {
                        return Vec::new();
                    }
                    vec![ActorCommand::SendSessionMessage(SessionActorMessage {
                        backend_id,
                        session_id,
                        actor: target.to_string(),
                        body,
                    })]
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

fn valid_editor_actor_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 128
        && target != "init"
        && target != NEOVIM_FILES_ID
        && target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn start_neovim_client(id: String, backend_id: u64, session_id: u64) -> ActorCommand {
    ActorCommand::StartActor(ActorStartRequest {
        id,
        path: NEOVIM_CLIENT_COMPONENT_FILE.to_string(),
        capabilities: vec![
            "runtime:package-cwd".to_string(),
            "subprocess:node".to_string(),
        ],
        backend_id: Some(backend_id),
        session_id: Some(session_id),
    })
}

impl EditorMessage {
    fn from_bytes(bytes: &[u8]) -> Self {
        serde_json::from_slice(bytes).unwrap_or_default()
    }
}

fn sender_actor_is(sender: &str, expected: &str) -> bool {
    sender.rsplit(':').next() == Some(expected)
}

fn show_diff(_state: &mut SwitcherState, backend_id: u64, session_id: u64) -> Vec<ActorCommand> {
    let Ok(body) = serde_json::to_vec(&serde_json::json!({
        "kind": "create-terminal",
        "command": "git --no-pager diff --color=always --no-ext-diff",
    })) else {
        return Vec::new();
    };
    vec![ActorCommand::SendSessionMessage(SessionActorMessage {
        backend_id,
        session_id,
        actor: "init".to_string(),
        body,
    })]
}

fn start_editor_agent_work(
    state: &mut SwitcherState,
    backend_id: u64,
    session_id: u64,
    requested_agent: &str,
    prompt: &str,
) -> Vec<ActorCommand> {
    let agent = if requested_agent.is_empty() {
        "coding-agent-1"
    } else {
        requested_agent
    };
    let (model, reasoning) = if (state.backend_id, state.session_id) == (backend_id, session_id) {
        state
            .agents
            .iter()
            .find(|candidate| candidate.id == agent)
            .map(|candidate| (candidate.model.clone(), candidate.reasoning.clone()))
            .unwrap_or_else(|| ("default".to_string(), "medium".to_string()))
    } else {
        ("default".to_string(), "medium".to_string())
    };
    let Ok(body) = serde_json::to_vec(&serde_json::json!({
        "kind": "prompt",
        "agent": agent,
        "prompt": prompt,
        "model": model,
        "reasoning": reasoning,
    })) else {
        return Vec::new();
    };
    let id = format!(
        "coding-agent-ui-b{backend_id}-s{session_id}-{}",
        state.next_coding_agent_ui
    );
    state.next_coding_agent_ui = state.next_coding_agent_ui.saturating_add(1);
    state
        .coding_agent_targets
        .insert(id.clone(), agent.to_string());
    vec![
        ActorCommand::SendSessionMessage(SessionActorMessage {
            backend_id,
            session_id,
            actor: agent.to_string(),
            body,
        }),
        start_coding_agent_ui(&id, backend_id, session_id),
    ]
}

fn neovim_ui_id(backend_id: u64, session_id: u64) -> String {
    format!("{NEOVIM_UI_ID_PREFIX}{backend_id}-s{session_id}")
}

fn request_editor(backend_id: u64, session_id: u64, kind: &str) -> ActorCommand {
    ActorCommand::SendSessionMessage(SessionActorMessage {
        backend_id,
        session_id,
        actor: "init".to_string(),
        body: format!(r#"{{"kind":"{kind}"}}"#).into_bytes(),
    })
}

fn query_editor_status(backend_id: u64, session_id: u64) -> ActorCommand {
    request_editor(backend_id, session_id, "editor-status")
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
        let backend = message.result.value.backend.unwrap_or_default();
        let request = state.next_request();
        return vec![backend_session_command(
            request,
            backend,
            "activate",
            Some(session),
            None,
            None,
        )];
    }
    if let Some(action) = message.result.value.action {
        match action.as_str() {
            "choose-directory" => {
                if let Some(backend) = message.result.value.backend {
                    return open_directory_picker(&mut state, backend);
                }
            }
            "create-directory" => {
                let Some(backend) = message.result.value.backend else {
                    return Vec::new();
                };
                let Some(directory) = message.result.value.directory else {
                    return Vec::new();
                };
                let request = state.next_request();
                let name = directory
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .filter(|name| !name.is_empty())
                    .map(str::to_string);
                return vec![backend_session_command(
                    request,
                    backend,
                    "create",
                    None,
                    name,
                    Some(directory),
                )];
            }
            "create-local" => {
                state.status = "Starting local backend…".to_string();
                return vec![ActorCommand::SendHostMessage(
                    br#"{"type":"create-local-backend"}"#.to_vec(),
                )];
            }
            "create-remote" => {
                state.query.clear();
                state.mode = EditMode::ConnectRemote;
                state.status = "SSH target (user@host): Enter to deploy and connect".to_string();
                redraw_or_record_error(&mut state);
                return Vec::new();
            }
            _ => {}
        }
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
        "coding-agent-ui-b{}-s{}-{}",
        state.backend_id, state.session_id, state.next_coding_agent_ui
    );
    state.next_coding_agent_ui = state.next_coding_agent_ui.saturating_add(1);
    state.coding_agent_targets.insert(id.clone(), agent);
    vec![start_coding_agent_ui(
        &id,
        state.backend_id,
        state.session_id,
    )]
}

fn open_session_picker(state: &mut SwitcherState) -> Vec<ActorCommand> {
    let items = state
        .sessions
        .iter()
        .map(|session| SessionPickerItem {
            label: session.name.clone(),
            detail: if session.windows == 1 {
                "1 window".to_string()
            } else {
                format!("{} windows", session.windows)
            },
            value: SessionPickerValueWire {
                backend: Some(session.backend),
                session: Some(session.id),
                action: None,
                directory: None,
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

fn open_creation_backend_picker(state: &mut SwitcherState) -> Vec<ActorCommand> {
    let mut items = state
        .sessions
        .iter()
        .map(|session| session.backend)
        .collect::<Vec<_>>();
    items.sort_unstable();
    items.dedup();
    let mut picker_items = Vec::new();
    for backend in items {
        let label = format!("Backend {backend}");
        picker_items.push(SessionPickerItem {
            label,
            detail: "Create a session on this backend".to_string(),
            value: SessionPickerValueWire {
                backend: Some(backend),
                session: None,
                action: Some("choose-directory".to_string()),
                directory: None,
            },
        });
    }
    if state.can_create_local {
        picker_items.push(SessionPickerItem {
            label: "Create local backend".to_string(),
            detail: "Start another local session server".to_string(),
            value: SessionPickerValueWire {
                backend: None,
                session: None,
                action: Some("create-local".to_string()),
                directory: None,
            },
        });
    }
    if state.can_connect_remote {
        picker_items.push(SessionPickerItem {
            label: "Connect remote backend".to_string(),
            detail: "Deploy and connect through your OpenSSH configuration".to_string(),
            value: SessionPickerValueWire {
                backend: None,
                session: None,
                action: Some("create-remote".to_string()),
                directory: None,
            },
        });
    }
    let request = SessionPickerRequest {
        message_type: "picker-create",
        title: "Choose backend",
        placeholder: "Fuzzy-match a backend or create a connection…",
        items: picker_items,
    };
    let Ok(body) = serde_json::to_vec(&request) else {
        state.error = true;
        state.status = "Could not encode the backend picker".to_string();
        redraw_or_record_error(state);
        return Vec::new();
    };
    vec![ActorCommand::SendLocalMessage(LocalActorMessage {
        actor: PICKER_ACTOR_ID.to_string(),
        body,
    })]
}

fn open_directory_picker(state: &mut SwitcherState, backend: u64) -> Vec<ActorCommand> {
    state.pending_directory_backend = None;
    let roots = state
        .sessions
        .iter()
        .filter(|session| session.backend == backend)
        .filter_map(|session| session.working_directory.clone())
        .collect::<Vec<_>>();
    let request = state.next_request();
    state.status = format!("Loading directories from backend {backend}…");
    let Ok(body) = serde_json::to_vec(&DirectoryListRequest {
        kind: "list-directories",
        request,
        roots,
    }) else {
        state.pending = None;
        state.error = true;
        state.status = "Could not encode the directory request".to_string();
        redraw_or_record_error(state);
        return Vec::new();
    };
    redraw_or_record_error(state);
    vec![ActorCommand::SendDaemonMessage(DaemonActorMessage {
        backend_id: backend,
        actor: "init".to_string(),
        body,
    })]
}

fn show_directory_picker(
    state: &mut SwitcherState,
    backend: u64,
    mut directories: Vec<String>,
) -> Vec<ActorCommand> {
    directories.extend(
        state
            .sessions
            .iter()
            .filter(|session| session.backend == backend)
            .filter_map(|session| session.working_directory.clone()),
    );
    directories.push("/".to_string());
    directories.sort();
    directories.dedup();
    let items = directories
        .into_iter()
        .map(|directory| SessionPickerItem {
            label: directory.clone(),
            detail: format!("Create a session on backend {backend}"),
            value: SessionPickerValueWire {
                backend: Some(backend),
                session: None,
                action: Some("create-directory".to_string()),
                directory: Some(directory),
            },
        })
        .collect();
    let request = SessionPickerRequest {
        message_type: "picker-create",
        title: "Choose directory",
        placeholder: "Fuzzy-match a directory…",
        items,
    };
    let Ok(body) = serde_json::to_vec(&request) else {
        state.error = true;
        state.status = "Could not encode the directory picker".to_string();
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

fn start_coding_agent_ui(id: &str, backend_id: u64, session_id: u64) -> ActorCommand {
    ActorCommand::StartActor(ActorStartRequest {
        id: id.to_string(),
        path: CODING_AGENT_COMPONENT_FILE.to_string(),
        capabilities: Vec::new(),
        backend_id: Some(backend_id),
        session_id: Some(session_id),
    })
}

fn bind_coding_agent_ui(
    id: &str,
    backend_id: u64,
    session_id: u64,
    agent: Option<&str>,
) -> ActorCommand {
    ActorCommand::SendLocalMessage(LocalActorMessage {
        actor: id.to_string(),
        body: format!(
            r#"{{"kind":"bind","backend_id":{backend_id},"session_id":{session_id},"agent":"{}"}}"#,
            agent.unwrap_or_default()
        )
        .into_bytes(),
    })
}

fn subscribe_agent_registry(backend_id: u64, session_id: u64) -> ActorCommand {
    ActorCommand::SendSessionMessage(SessionActorMessage {
        backend_id,
        session_id,
        actor: "init".to_string(),
        body: br#"{"kind":"registry-subscribe"}"#.to_vec(),
    })
}

fn ensure_default_terminal_ui(
    state: &mut SwitcherState,
    backend_id: u64,
    session_id: u64,
) -> Option<ActorCommand> {
    if backend_id == 0
        || session_id == 0
        || state
            .terminal_uis
            .values()
            .any(|realm| *realm == (backend_id, session_id))
    {
        return None;
    }
    let id = if state.terminal_uis.is_empty() {
        TERMINAL_UI_ID.to_string()
    } else {
        format!("terminal-ui-b{backend_id}-s{session_id}")
    };
    state
        .terminal_uis
        .insert(id.clone(), (backend_id, session_id));
    Some(ActorCommand::StartActor(ActorStartRequest {
        id,
        path: TERMINAL_UI_COMPONENT_FILE.to_string(),
        capabilities: Vec::new(),
        backend_id: Some(backend_id),
        session_id: Some(session_id),
    }))
}

fn actor_realm(actor: &str) -> Option<(u64, u64)> {
    let backend = actor.split("-b").nth(1)?.split('-').next()?.parse().ok()?;
    let session = actor.split("-s").nth(1)?.split('-').next()?.parse().ok()?;
    Some((backend, session))
}

fn neovim_client_realm(actor: &str) -> Option<(u64, u64)> {
    let address = actor.strip_prefix(NEOVIM_UI_ID_PREFIX)?;
    let (backend, session) = address.split_once("-s")?;
    let realm = (backend.parse().ok()?, session.parse().ok()?);
    (neovim_ui_id(realm.0, realm.1) == actor).then_some(realm)
}

fn authenticated_neovim_client(state: &SwitcherState, actor: &str) -> Option<(u64, u64)> {
    let realm = neovim_client_realm(actor)?;
    (state.neovim_uis.get(&realm).map(String::as_str) == Some(actor)).then_some(realm)
}

fn remove_neovim_ui(state: &mut SwitcherState, actor: &str) -> Option<(u64, u64)> {
    let realm = state
        .neovim_uis
        .iter()
        .find_map(|(realm, current)| (current == actor).then_some(*realm))?;
    state.neovim_uis.remove(&realm);
    Some(realm)
}

fn remove_neovim_ui_any(state: &mut SwitcherState, actor: &str) -> Option<(u64, u64)> {
    remove_neovim_ui(state, actor).or_else(|| {
        let realm = state
            .neovim_uis_starting
            .iter()
            .find_map(|(realm, current)| (current == actor).then_some(*realm))?;
        state.neovim_uis_starting.remove(&realm);
        Some(realm)
    })
}

fn remove_neovim_ui_realm(state: &mut SwitcherState, realm: (u64, u64)) -> Option<String> {
    state
        .neovim_uis
        .remove(&realm)
        .or_else(|| state.neovim_uis_starting.remove(&realm))
}

fn editor_client_lifecycle(
    kind: &str,
    actor: &str,
    backend_id: u64,
    session_id: u64,
    message: &str,
) -> ActorCommand {
    let body = serde_json::to_vec(&serde_json::json!({
        "kind": kind,
        "session_id": session_id,
        "actor": actor,
        "message": message.chars().take(2_048).collect::<String>(),
    }))
    .unwrap_or_else(|_| br#"{"kind":"editor-client-failed"}"#.to_vec());
    ActorCommand::SendSessionMessage(SessionActorMessage {
        backend_id,
        session_id,
        actor: "init".to_string(),
        body,
    })
}

fn stop_terminal_command(backend_id: u64, session_id: u64, ui_actor: &str) -> Option<ActorCommand> {
    let terminal = if ui_actor == TERMINAL_UI_ID
        || ui_actor == format!("terminal-ui-b{backend_id}-s{session_id}")
    {
        "terminal-session".to_string()
    } else {
        let suffix = ui_actor.splitn(5, '-').nth(4)?;
        format!("terminal-session-{suffix}")
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "kind": "stop-terminal",
        "terminal": terminal,
    }))
    .ok()?;
    Some(ActorCommand::SendSessionMessage(SessionActorMessage {
        backend_id,
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
    let inbox_pane = state.inbox_pane;
    let delegated = message
        .visible_panes
        .iter()
        .copied()
        .filter(|visible| {
            *visible != pane && Some(*visible) != picker_pane && Some(*visible) != inbox_pane
        })
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
    let inbox_visible = inbox_pane.is_some_and(|inbox| message.visible_panes.contains(&inbox));
    let width = f64::from(message.size.w.max(1));
    let mut sidebar_fraction = if pane_visible {
        (SIDEBAR_WIDTH / width).clamp(0.18, 0.38)
    } else {
        0.0
    };
    let mut inbox_fraction = if inbox_visible {
        (INBOX_WIDTH / width).clamp(0.18, 0.38)
    } else {
        0.0
    };
    if has_rest {
        let chrome = sidebar_fraction + inbox_fraction;
        if chrome > 0.68 {
            let scale = 0.68 / chrome;
            sidebar_fraction *= scale;
            inbox_fraction *= scale;
        }
    } else {
        let chrome = sidebar_fraction + inbox_fraction;
        if chrome > 0.0 {
            sidebar_fraction /= chrome;
            inbox_fraction /= chrome;
        }
    }
    let rest_fraction = (1.0 - sidebar_fraction - inbox_fraction).max(0.0);
    let sidebar_offset = -1.0 + 2.0 * rest_fraction;
    let sidebar_size = 2.0 * sidebar_fraction;
    let inbox_offset = 1.0 - 2.0 * inbox_fraction;
    let inbox_size = 2.0 * inbox_fraction;
    let rest_size = 2.0 * rest_fraction;

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
    if inbox_visible && let Some(inbox) = inbox_pane {
        layout_items.push(format!(
            "{{\"id\":{},\"kind\":{{\"type\":\"pane\",\"pane\":{}}},\"z\":2,\"transform\":{{\"offset\":{{\"u\":{},\"v\":-1}},\"size\":{{\"u\":{},\"v\":2}}}},\"input\":\"pane\"}}",
            inbox, inbox, inbox_offset, inbox_size
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

fn switcher_target_at(state: &SwitcherState, x: f32, y: f32) -> Option<SwitcherTarget> {
    if !x.is_finite() || !y.is_finite() || x < 0.0 || x >= state.width {
        return None;
    }
    let header = header_geometry(state.width);
    if (10.0..42.0).contains(&y)
        && x >= header.button_x
        && x < header.button_x + header.button_width
        && state.pending.is_none()
    {
        return Some(SwitcherTarget::NewSession);
    }
    if y < HEADER_HEIGHT || y >= state.height - FOOTER_HEIGHT {
        return None;
    }
    let items = state.list_items();
    let mut item_y = HEADER_HEIGHT;
    let item = items.iter().skip(state.scroll).find(|item| {
        let height = item_height(item);
        let hit = y >= item_y && y < item_y + height;
        item_y += height;
        hit
    });
    let item = item.cloned()?;
    if let ListItem::Project(project) = item {
        if x >= state.width - 48.0 && project.git {
            Some(SwitcherTarget::ProjectWorktree(project.backend, project.id))
        } else {
            Some(SwitcherTarget::ProjectToggle(project.backend, project.id))
        }
    } else if let ListItem::Session(index) = item {
        let session = state.sessions[index].clone();
        if x >= state.width - 48.0 {
            Some(SwitcherTarget::SessionClose(session.backend, session.id))
        } else {
            Some(SwitcherTarget::Session(session.backend, session.id))
        }
    } else {
        None
    }
}

fn activate_switcher_target(
    state: &mut SwitcherState,
    target: SwitcherTarget,
) -> Vec<ActorCommand> {
    match target {
        SwitcherTarget::NewSession => open_creation_backend_picker(state),
        SwitcherTarget::ProjectToggle(backend, project) => {
            let project_key = format!("{backend}:{project}");
            if !state.collapsed_projects.remove(&project_key) {
                state.collapsed_projects.insert(project_key);
            }
            state.scroll = state.scroll.min(state.list_items().len().saturating_sub(1));
            state.ensure_selected_visible();
            Vec::new()
        }
        SwitcherTarget::ProjectWorktree(backend, project) => {
            if let Some(project) = state
                .project_groups()
                .into_iter()
                .find(|candidate| candidate.backend == backend && candidate.id == project)
            {
                begin_worktree(state, &project);
            }
            Vec::new()
        }
        SwitcherTarget::Session(backend, session) => {
            let Some(session) = state
                .sessions
                .iter()
                .find(|candidate| candidate.backend == backend && candidate.id == session)
                .cloned()
            else {
                return Vec::new();
            };
            state.selected = Some((session.backend, session.id));
            let request = state.next_request();
            vec![backend_session_command(
                request,
                session.backend,
                "activate",
                Some(session.id),
                None,
                None,
            )]
        }
        SwitcherTarget::SessionClose(backend, session) => {
            state.selected = Some((backend, session));
            close_selected(state)
        }
    }
}

fn handle_secondary_click(state: &mut SwitcherState, x: f32, y: f32) -> Vec<ActorCommand> {
    let Some(target) = switcher_target_at(state, x, y) else {
        return Vec::new();
    };
    let (backend, session) = match target {
        SwitcherTarget::Session(backend, session)
        | SwitcherTarget::SessionClose(backend, session) => (backend, session),
        _ => return Vec::new(),
    };
    if let Some(session) = state
        .sessions
        .iter()
        .find(|candidate| candidate.backend == backend && candidate.id == session)
        .cloned()
    {
        state.selected = Some((backend, session.id));
        begin_rename(state, &session);
    }
    Vec::new()
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
    if let EditMode::Rename(backend, session) = &state.mode {
        let backend = *backend;
        let session = *session;
        let name = state.query.trim().to_string();
        if name.is_empty() {
            state.error = true;
            state.status = "A session name cannot be empty".to_string();
            return Vec::new();
        }
        let request = state.next_request();
        return vec![backend_session_command(
            request,
            backend,
            "rename",
            Some(session),
            Some(name),
            None,
        )];
    }
    if let EditMode::CreateWorktree(backend, session) = &state.mode {
        let backend = *backend;
        let session = *session;
        let branch = state.query.trim().to_string();
        if branch.is_empty() {
            state.error = true;
            state.status = "A branch name cannot be empty".to_string();
            return Vec::new();
        }
        let request = state.next_request();
        let Ok(body) = serde_json::to_vec(&WorktreeSessionRequest {
            kind: "create-worktree-session",
            request,
            source_session: session,
            branch: &branch,
        }) else {
            state.pending = None;
            state.error = true;
            state.status = "Could not encode the worktree request".to_string();
            return Vec::new();
        };
        return vec![ActorCommand::SendDaemonMessage(DaemonActorMessage {
            backend_id: backend,
            actor: "init".to_string(),
            body,
        })];
    }
    if state.mode == EditMode::ConnectRemote {
        let address = state.query.trim().to_string();
        if address.is_empty() {
            state.error = true;
            state.status = "Enter an SSH target such as user@host".to_string();
            return Vec::new();
        }
        state.error = false;
        state.status = format!("Deploying {address} over SSH…");
        state.next_request();
        return vec![ActorCommand::SendHostMessage(
            format!("{{\"type\":\"deploy-ssh-backend\",\"target\":{address:?}}}").into_bytes(),
        )];
    }
    let visible_sessions = state
        .list_items()
        .into_iter()
        .filter_map(|item| match item {
            ListItem::Project(_) => None,
            ListItem::Session(index) => {
                Some((state.sessions[index].backend, state.sessions[index].id))
            }
        })
        .collect::<Vec<_>>();
    if let Some(selected) = state.selected
        && visible_sessions.contains(&selected)
    {
        let request = state.next_request();
        return vec![backend_session_command(
            request,
            selected.0,
            "activate",
            Some(selected.1),
            None,
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
    let backend = state
        .selected_session()
        .map_or(state.backend_id, |session| session.backend);
    if backend == 0 {
        state.error = true;
        state.status = "No backend is available for the new session".to_string();
        return Vec::new();
    }
    vec![backend_session_command(
        request,
        backend,
        "create",
        None,
        Some(name),
        None,
    )]
}

fn close_selected(state: &mut SwitcherState) -> Vec<ActorCommand> {
    let Some((backend, session)) = state.selected else {
        return Vec::new();
    };
    if state.sessions.len() <= 1 {
        state.error = true;
        state.status = "The last session cannot be closed".to_string();
        return Vec::new();
    }
    let request = state.next_request();
    vec![backend_session_command(
        request,
        backend,
        "close",
        Some(session),
        None,
        None,
    )]
}

fn begin_rename(state: &mut SwitcherState, session: &ViewSession) {
    state.mode = EditMode::Rename(session.backend, session.id);
    state.query.clone_from(&session.name);
    state.status = "Rename: Enter to save, Escape to cancel".to_string();
    state.error = false;
}

fn begin_worktree(state: &mut SwitcherState, project: &ProjectGroup) {
    state.mode = EditMode::CreateWorktree(project.backend, project.source_session);
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
            visible.iter().position(|index| {
                (state.sessions[*index].backend, state.sessions[*index].id) == selected
            })
        })
        .unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(visible.len() - 1)
    };
    state.selected = Some((
        state.sessions[visible[next]].backend,
        state.sessions[visible[next]].id,
    ));
    state.ensure_selected_visible();
}

fn select_first_filtered(state: &mut SwitcherState) {
    state.selected = state.list_items().into_iter().find_map(|item| match item {
        ListItem::Project(_) => None,
        ListItem::Session(index) => Some((state.sessions[index].backend, state.sessions[index].id)),
    });
}

fn session_command(
    backend_id: u64,
    request: u64,
    operation: SessionOperation,
    session: Option<u64>,
    name: Option<String>,
    directory: Option<String>,
) -> ActorCommand {
    ActorCommand::SessionCommand(SessionCommand {
        backend_id,
        request,
        operation,
        session,
        name,
        directory,
    })
}

fn backend_session_command(
    request: u64,
    backend: u64,
    operation: &str,
    session: Option<u64>,
    name: Option<String>,
    directory: Option<String>,
) -> ActorCommand {
    let operation = match operation {
        "refresh" => SessionOperation::Refresh,
        "activate" => SessionOperation::Activate,
        "create" => SessionOperation::Create,
        "rename" => SessionOperation::Rename,
        "close" => SessionOperation::Close,
        _ => return ActorCommand::LifecycleComplete(Err("unknown session operation".to_string())),
    };
    session_command(backend, request, operation, session, name, directory)
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
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, HEADER_HEIGHT),
        color: SURFACE,
    });
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, HEADER_HEIGHT - 1.0, state.width, 1.0),
        color: BORDER,
    });

    let header = header_geometry(state.width);
    add_text(
        state,
        &mut packet,
        &mut resources,
        if header.narrow_title { "S" } else { "Sessions" },
        17.0,
        TEXT,
        SIDE_MARGIN,
        14.0,
        (header.input_x - SIDE_MARGIN * 2.0).max(12.0),
    )?;

    let input_color = if state.error {
        RED
    } else if state.mode == EditMode::Search {
        BORDER
    } else {
        YELLOW
    };
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(header.input_x, 10.0, header.input_width, 32.0),
        radius: 4.0,
        color: SURFACE_RAISED,
    });
    packet.push(Command::StrokeRoundedRect {
        rect: Rect::new(header.input_x, 10.0, header.input_width, 32.0),
        radius: 4.0,
        color: input_color,
        width: 1.0,
    });
    let prompt = if state.query.is_empty() {
        match &state.mode {
            EditMode::Search if header.input_width < 132.0 => "Search…".to_string(),
            EditMode::Search => "Search or create…".to_string(),
            EditMode::Rename(..) => "New session name…".to_string(),
            EditMode::CreateWorktree(..) => "Branch name…".to_string(),
            EditMode::ConnectRemote => "user@host…".to_string(),
        }
    } else {
        format!("{}▏", state.query)
    };
    add_text(
        state,
        &mut packet,
        &mut resources,
        &truncate_to_width(&prompt, header.input_width - 16.0, 14.0),
        14.0,
        if state.query.is_empty() { MUTED } else { FG },
        header.input_x + 8.0,
        16.0,
        header.input_width - 16.0,
    )?;

    let new_target = SwitcherTarget::NewSession;
    let new_pressed = state.pressed.as_ref() == Some(&new_target);
    let new_hovered = state.hovered.as_ref() == Some(&new_target);
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(header.button_x, 10.0, header.button_width, 32.0),
        radius: 4.0,
        color: if state.pending.is_some() {
            SURFACE_RAISED
        } else if new_pressed {
            PRESSED
        } else if new_hovered {
            ashen::ACCENT_GLOW
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
            if header.compact_button { "+" } else { "+ New" }
        } else {
            if header.compact_button { "+" } else { "Create" }
        },
        11.0,
        if state.pending.is_some() { MUTED } else { BG },
        header.button_x + 5.0,
        if new_pressed { 17.0 } else { 16.0 },
        header.button_width - 10.0,
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
                let toggle_target =
                    SwitcherTarget::ProjectToggle(project.backend, project.id.clone());
                let worktree_target =
                    SwitcherTarget::ProjectWorktree(project.backend, project.id.clone());
                let toggle_pressed = state.pressed.as_ref() == Some(&toggle_target);
                let toggle_hovered = state.hovered.as_ref() == Some(&toggle_target);
                packet.push(Command::FillRect {
                    rect: Rect::new(0.0, y, state.width, PROJECT_ROW_HEIGHT),
                    color: if toggle_pressed {
                        PRESSED
                    } else if toggle_hovered {
                        HOVER
                    } else {
                        SURFACE
                    },
                });
                packet.push(Command::FillRect {
                    rect: Rect::new(0.0, y + PROJECT_ROW_HEIGHT - 1.0, state.width, 1.0),
                    color: BORDER,
                });
                let collapsed = state
                    .collapsed_projects
                    .contains(&format!("{}:{}", project.backend, project.id));
                add_text(
                    state,
                    &mut packet,
                    &mut resources,
                    if collapsed { "▸" } else { "▾" },
                    12.0,
                    MUTED,
                    SIDE_MARGIN,
                    y + 8.0,
                    12.0,
                )?;
                add_text(
                    state,
                    &mut packet,
                    &mut resources,
                    &truncate_to_width(&project.name, state.width - 126.0, 13.0),
                    13.0,
                    TEXT,
                    SIDE_MARGIN + 16.0,
                    y + 8.0,
                    state.width - 126.0,
                )?;
                add_text(
                    state,
                    &mut packet,
                    &mut resources,
                    &project.sessions.len().to_string(),
                    10.0,
                    MUTED,
                    state.width - 76.0,
                    y + 9.0,
                    24.0,
                )?;
                if project.git {
                    let worktree_pressed = state.pressed.as_ref() == Some(&worktree_target);
                    let worktree_hovered = state.hovered.as_ref() == Some(&worktree_target);
                    if worktree_pressed || worktree_hovered {
                        packet.push(Command::FillRoundedRect {
                            rect: Rect::new(state.width - 44.0, y + 4.0, 32.0, 26.0),
                            radius: 4.0,
                            color: if worktree_pressed { PRESSED } else { HOVER },
                        });
                    }
                    add_text(
                        state,
                        &mut packet,
                        &mut resources,
                        "+",
                        16.0,
                        YELLOW,
                        state.width - 36.0,
                        y + 5.0,
                        20.0,
                    )?;
                }
            }
            ListItem::Session(index) => {
                let session = state.sessions[*index].clone();
                let selected = state.selected == Some((session.backend, session.id));
                let row_target = SwitcherTarget::Session(session.backend, session.id);
                let close_target = SwitcherTarget::SessionClose(session.backend, session.id);
                let row_pressed = state.pressed.as_ref() == Some(&row_target);
                let row_hovered = state.hovered.as_ref() == Some(&row_target);
                let name_x = SIDE_MARGIN + 12.0;
                let close_x = state.width - 38.0;
                let show_window_count = state.width >= 220.0;
                let count_x = state.width - 116.0;
                let name_right = if show_window_count {
                    count_x - 8.0
                } else {
                    close_x - 8.0
                };
                let name_width = (name_right - name_x).max(1.0);
                if row_pressed || row_hovered || session.active || selected {
                    packet.push(Command::FillRect {
                        rect: Rect::new(0.0, y, state.width, ROW_HEIGHT),
                        color: if row_pressed {
                            PRESSED
                        } else if row_hovered {
                            HOVER
                        } else if session.active {
                            ashen::SELECTION
                        } else {
                            ACTIVE
                        },
                    });
                }
                if session.active || selected {
                    packet.push(Command::FillRect {
                        rect: Rect::new(0.0, y, 3.0, ROW_HEIGHT),
                        color: YELLOW,
                    });
                }
                packet.push(Command::FillRect {
                    rect: Rect::new(0.0, y + ROW_HEIGHT - 1.0, state.width, 1.0),
                    color: Color::rgba(BORDER.r, BORDER.g, BORDER.b, 0.38),
                });
                let close_pressed = state.pressed.as_ref() == Some(&close_target);
                let close_hovered = state.hovered.as_ref() == Some(&close_target);
                if close_pressed || close_hovered {
                    packet.push(Command::FillRoundedRect {
                        rect: Rect::new(close_x - 6.0, y + 3.0, 30.0, 24.0),
                        radius: 4.0,
                        color: if close_pressed { PRESSED } else { HOVER },
                    });
                }
                add_text(
                    state,
                    &mut packet,
                    &mut resources,
                    &truncate_to_width(&session.name, name_width, 13.0),
                    13.0,
                    FG,
                    name_x,
                    y + 5.0,
                    name_width,
                )?;
                if show_window_count {
                    let count = if state.width < 300.0 {
                        format!("{} win", session.windows)
                    } else if session.windows == 1 {
                        "1 window".to_string()
                    } else {
                        format!("{} windows", session.windows)
                    };
                    add_text(
                        state,
                        &mut packet,
                        &mut resources,
                        &count,
                        10.0,
                        if session.active { TEXT } else { MUTED },
                        count_x,
                        y + 6.0,
                        70.0,
                    )?;
                }
                add_text(
                    state,
                    &mut packet,
                    &mut resources,
                    "×",
                    16.0,
                    RED,
                    close_x,
                    y + 2.0,
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
            color: Color::rgba(FG.r, FG.g, FG.b, 0.42),
        });
    }

    packet.push(Command::FillRect {
        rect: Rect::new(
            0.0,
            state.height - FOOTER_HEIGHT,
            state.width,
            FOOTER_HEIGHT,
        ),
        color: SURFACE,
    });
    let footer = if !state.status.is_empty() {
        state.status.clone()
    } else if let Some(target) = state.hovered.as_ref() {
        match target {
            SwitcherTarget::NewSession => "Create or connect a session".to_string(),
            SwitcherTarget::ProjectToggle(..) => "Expand or collapse this project".to_string(),
            SwitcherTarget::ProjectWorktree(..) => {
                "Create a worktree session in this project".to_string()
            }
            SwitcherTarget::Session(..) => "Click to open this session".to_string(),
            SwitcherTarget::SessionClose(..) => "Close this session".to_string(),
        }
    } else if items.is_empty() {
        "No match — Enter creates it".to_string()
    } else {
        "Enter open  ·  F2 rename  ·  Delete close".to_string()
    };
    add_text(
        state,
        &mut packet,
        &mut resources,
        &truncate_to_width(&footer, state.width - 28.0, 10.0),
        10.0,
        if state.error { RED } else { MUTED },
        SIDE_MARGIN,
        state.height - 21.0,
        state.width - 28.0,
    )?;

    submit_packet(state.pane, &packet)?;
    drop_resources(state.pane, state.text_resources.drain(..));
    state.text_resources = resources;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct HeaderGeometry {
    input_x: f32,
    input_width: f32,
    button_x: f32,
    button_width: f32,
    compact_button: bool,
    narrow_title: bool,
}

fn header_geometry(width: f32) -> HeaderGeometry {
    let compact_button = width < 260.0;
    let narrow_title = width < 220.0;
    let button_width = if compact_button { 32.0 } else { 42.0 };
    let button_x = (width - SIDE_MARGIN - button_width).max(SIDE_MARGIN);
    let input_x = if narrow_title { 36.0 } else { 94.0 };
    let input_width = (button_x - input_x - 8.0).max(1.0);
    HeaderGeometry {
        input_x,
        input_width,
        button_x,
        button_width,
        compact_button,
        narrow_title,
    }
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
    let layout = layout_ui_text(state.pane, text, size, max_width, size + 4.0, color)
        .map_err(|error| format!("layout session switcher text: {error}"))?;
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

bindings::export!(SessionSwitcher with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::{
        ActorCommand, SwitcherState, actor_realm, authenticated_neovim_client,
        ensure_default_terminal_ui, header_geometry, neovim_client_realm, neovim_ui_id,
        request_editor, sender_actor_is, show_diff, start_neovim_client, truncate_to_width,
        valid_editor_actor_target,
    };

    #[test]
    fn narrow_header_geometry_never_overlaps_the_action_button() {
        for width in [180.0, 220.0, 252.0, 320.0] {
            let header = header_geometry(width);
            assert!(header.input_width > 0.0);
            assert!(header.input_x + header.input_width <= header.button_x);
        }
    }

    #[test]
    fn narrow_session_names_are_ellipsized_to_one_row() {
        assert_eq!(
            truncate_to_width("drift-bay-pulse-909d", 92.0, 13.0),
            "drift-bay-p…"
        );
    }

    #[test]
    fn each_backend_active_session_gets_one_default_terminal_frontend() {
        let mut state = SwitcherState::new();
        let Some(ActorCommand::StartActor(first)) = ensure_default_terminal_ui(&mut state, 1, 7)
        else {
            panic!("first backend did not start a terminal frontend");
        };
        assert_eq!(first.id, "terminal-ui");

        let Some(ActorCommand::StartActor(second)) = ensure_default_terminal_ui(&mut state, 2, 7)
        else {
            panic!("second backend did not start a terminal frontend");
        };
        assert_eq!(second.id, "terminal-ui-b2-s7");
        assert!(ensure_default_terminal_ui(&mut state, 2, 7).is_none());
    }

    #[test]
    fn neovim_frontend_identity_round_trips_its_backend_realm() {
        let id = neovim_ui_id(4, 27);
        assert_eq!(id, "neovim-ui-b4-s27");
        assert_eq!(actor_realm(&id), Some((4, 27)));
        assert_eq!(neovim_client_realm(&id), Some((4, 27)));
        assert_eq!(neovim_client_realm("neovim-ui-b4-s27-spoof"), None);
    }

    #[test]
    fn neovim_messages_require_the_exact_recorded_child_identity() {
        let mut state = SwitcherState::new();
        state
            .neovim_uis_starting
            .insert((4, 27), "neovim-ui-b4-s27".to_string());
        assert_eq!(
            authenticated_neovim_client(&state, "neovim-ui-b4-s27"),
            None
        );
        state.neovim_uis_starting.clear();
        state
            .neovim_uis
            .insert((4, 27), "neovim-ui-b4-s27".to_string());
        assert_eq!(
            authenticated_neovim_client(&state, "neovim-ui-b4-s27"),
            Some((4, 27))
        );
        assert_eq!(
            authenticated_neovim_client(&state, "neovim-ui-b4-s27-spoof"),
            None
        );
        state
            .neovim_uis
            .insert((5, 27), "neovim-ui-b5-s27-other".to_string());
        assert_eq!(
            authenticated_neovim_client(&state, "neovim-ui-b5-s27"),
            None
        );
    }

    #[test]
    fn neovim_frontend_runs_the_client_local_wasm_runtime() {
        let ActorCommand::StartActor(started) =
            start_neovim_client("neovim-ui-b4-s27".to_string(), 4, 27)
        else {
            panic!("Neovim client was not started as a presentation actor");
        };
        assert_eq!(started.path, "brood_neovim_client_plugin.component.wasm");
        assert_eq!(
            started.capabilities,
            ["runtime:package-cwd", "subprocess:node"]
        );
        assert_eq!(started.backend_id, Some(4));
        assert_eq!(started.session_id, Some(27));
    }

    #[test]
    fn generic_neovim_messages_stay_in_the_selected_session_actor_namespace() {
        assert!(valid_editor_actor_target("coding-agent-1"));
        assert!(valid_editor_actor_target("my.plugin_actor"));
        assert!(!valid_editor_actor_target("init"));
        assert!(!valid_editor_actor_target("neovim-files"));
        assert!(!valid_editor_actor_target("presentation:key-policy"));
        assert!(!valid_editor_actor_target("../actor"));
    }

    #[test]
    fn editor_lifecycle_sender_must_be_the_session_init_actor() {
        assert!(sender_actor_is("session:4:init", "init"));
        assert!(!sender_actor_is("session:4:terminal-session", "init"));
        assert!(!sender_actor_is("session:4:not-init", "init"));
    }

    #[test]
    fn editor_request_targets_the_selected_session_init_actor() {
        let ActorCommand::SendSessionMessage(message) = request_editor(8, 13, "create-editor")
        else {
            panic!("editor request did not use the session mailbox");
        };
        assert_eq!(message.backend_id, 8);
        assert_eq!(message.session_id, 13);
        assert_eq!(message.actor, "init");
        assert_eq!(message.body, br#"{"kind":"create-editor"}"#);
    }

    #[test]
    fn editor_diff_request_runs_git_in_the_remote_session() {
        let mut state = SwitcherState::new();
        let commands = show_diff(&mut state, 3, 9);
        let [ActorCommand::SendSessionMessage(message)] = commands.as_slice() else {
            panic!("diff request did not use the session mailbox");
        };
        assert_eq!(message.backend_id, 3);
        assert_eq!(message.session_id, 9);
        assert_eq!(message.actor, "init");
        assert!(
            String::from_utf8_lossy(&message.body)
                .contains("git --no-pager diff --color=always --no-ext-diff")
        );
    }
}
