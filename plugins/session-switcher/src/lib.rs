use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use brood_graphics::{Color, Command, Packet, Rect, theme::ashen};
use brood_plugin_sdk::{
    actor::{start_global_actor, start_session_actor},
    bindings,
    exports::brood::plugin::actor::{
        ChildEvent, Command as ActorCommand, DaemonActorMessage, Event, Guest, LifecycleEvent,
        LocalActorMessage, PresentationEvent, SessionActorMessage, SessionCommand,
        SessionCommandCompleted, SessionEvent, SessionOperation, SessionSnapshot,
    },
    graphics::{TextLayoutCache, reuse_pane_content},
    input::{BUTTON_PRIMARY as LEFT_BUTTON, BUTTON_SECONDARY as RIGHT_BUTTON},
    layout::{
        Axis, DelegateLayoutRest, DividerItem, InputZone, ItemKind, LayoutFragment, LayoutItem,
        LayoutRest, Lease, ResolvedLayout, SetPointerPolicy, SetResolvedLayout, SubmitLayoutRegion,
        SubmitSynchronizedLayout, Transform,
    },
    text::truncate_to_width,
};
use brood_protocol::backend_catalogue::{BackendCatalogueEntry, BackendCatalogueMessage};
use serde::{Deserialize, Serialize};

const INIT_ACTOR_ID: &str = "brood.client-init";
const CHILD_ACTOR_ID: &str = "session-switcher";
const PICKER_ACTOR_ID: &str = "picker";
const BACKEND_PICKER_REQUEST: &str = "backend-discovery";
const KEYBINDINGS_ACTOR_ID: &str = "keybindings";
const INBOX_ACTOR_ID: &str = "inbox";
const STATUS_BAR_ACTOR_ID: &str = "status-bar";
const COMPONENT_FILE: &str = "brood_session_switcher_plugin.component.wasm";
const PICKER_COMPONENT_FILE: &str = "brood_picker_plugin.component.wasm";
const KEYBINDINGS_COMPONENT_FILE: &str = "brood_keybindings_plugin.component.wasm";
const INBOX_COMPONENT_FILE: &str = "brood_inbox_plugin.component.wasm";
const STATUS_BAR_COMPONENT_FILE: &str = "brood_status_bar_plugin.component.wasm";
const CODING_AGENT_COMPONENT_FILE: &str = "brood_coding_agent_plugin.component.wasm";
const DIFF_VIEWER_COMPONENT_FILE: &str = "brood_diff_viewer_plugin.component.wasm";
const NEOVIM_CLIENT_COMPONENT_FILE: &str = "brood_neovim_client_plugin.component.wasm";
const TERMINAL_UI_COMPONENT_FILE: &str = "brood_terminal_ui_plugin.component.wasm";
const TERMINAL_UI_ID: &str = "terminal-ui";
const DIFF_VIEWER_ID_PREFIX: &str = "diff-viewer-s";
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
const MIN_SHELL_PANE_WIDTH: f64 = 160.0;
const MIN_MAIN_AREA_WIDTH: f64 = 240.0;
const PRIMARY_BUTTON: u32 = 0x110;
// These are stable lease-local layout identities. Keep them outside both
// pane-ID allocator ranges so shell chrome cannot collide with pane items.
const SIDEBAR_DIVIDER_ZONE_ID: u64 = 0x8000_4252_4f4f_5301;
const INBOX_DIVIDER_ZONE_ID: u64 = 0x8000_4252_4f4f_5302;
const SIDEBAR_DIVIDER_ITEM_ID: u64 = 0x8000_4252_4f4f_5311;
const INBOX_DIVIDER_ITEM_ID: u64 = 0x8000_4252_4f4f_5312;
const DIVIDER_EVENT_MASK: u32 = 0b1111;
const STATUS_BAR_HEIGHT: f64 = 34.0;
const MAX_PENDING_LAYOUT_REQUESTS: usize = 64;
const MAX_NATIVE_RESIZE_RETRIES: u8 = 2;

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
    CreateLocalContainer,
    CreateRemoteContainer,
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
    can_create_container: bool,
    pane: u64,
    backend_id: u64,
    session_id: u64,
    sidebar_visible: bool,
    width: f32,
    height: f32,
    resize_extent: (f32, f32, f32),
    sessions: Vec<ViewSession>,
    backends: Vec<(u64, String)>,
    collapsed_projects: BTreeSet<String>,
    selected: Option<(u64, u64)>,
    query: String,
    mode: EditMode,
    scroll: usize,
    request: u64,
    pending: Option<u64>,
    pending_directory_backend: Option<u64>,
    awaiting_backend_catalogue: bool,
    backend_picker: Option<u64>,
    status: String,
    error: bool,
    next_coding_agent_ui: u64,
    coding_agent_uis: Vec<String>,
    coding_agent_targets: BTreeMap<String, String>,
    agents: Vec<WireAgent>,
    agent_picker_pending: bool,
    diff_viewers: BTreeSet<String>,
    terminal_uis: BTreeMap<String, (u64, u64)>,
    neovim_uis_starting: BTreeMap<(u64, u64), String>,
    neovim_uis: BTreeMap<(u64, u64), String>,
    text_cache: Option<TextLayoutCache>,
    layout: Option<LayoutContext>,
    workspace_focus: Option<u64>,
    picker_pane: Option<u64>,
    picker_visible: bool,
    keybindings_pane: Option<u64>,
    keybindings_visible: bool,
    inbox_pane: Option<u64>,
    inbox_visible: bool,
    sidebar_width: f64,
    inbox_width: f64,
    status_bar_pane: Option<u64>,
    next_layout_request: u64,
    native_resize_revision: Option<u64>,
    durable_shell_layout: Option<ShellLayoutState>,
    pending_shell_layouts: BTreeMap<u64, PendingShellLayout>,
    deferred_overlay_layout: Option<DeferredOverlayLayout>,
    pointer_drag: Option<ShellDividerDrag>,
    hovered: Option<SwitcherTarget>,
    pressed: Option<SwitcherTarget>,
}

#[derive(Clone, Deserialize)]
struct LayoutStateMessage {
    #[serde(rename = "type")]
    message_type: serde_json_core::heapless::String<32>,
    revision: u64,
    #[serde(default)]
    presented_layout_revision: Option<u64>,
    #[serde(default)]
    native_resize_revision: Option<u64>,
    lease: LayoutLeaseToken,
    #[serde(default, alias = "waitable_plugin_panes")]
    waitable_panes: serde_json_core::heapless::Vec<u64, 64>,
    #[serde(default)]
    presented_panes: serde_json_core::heapless::Vec<u64, 64>,
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

#[derive(Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
struct LayoutLeaseToken {
    id: u64,
    generation: u64,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct LayoutStateSize {
    w: i32,
    #[serde(rename = "h")]
    height: i32,
}

#[derive(Clone)]
struct LayoutContext {
    revision: u64,
    presented_layout_revision: u64,
    lease: LayoutLeaseToken,
    waitable_panes: serde_json_core::heapless::Vec<u64, 64>,
    presented_panes: serde_json_core::heapless::Vec<u64, 64>,
    visible_panes: serde_json_core::heapless::Vec<u64, 64>,
    focused_pane: Option<u64>,
    size: LayoutStateSize,
    rest: bool,
}

#[derive(Deserialize)]
struct LayoutPointerMessage {
    #[serde(rename = "type")]
    message_type: serde_json_core::heapless::String<32>,
    interaction: Option<u64>,
    event_sequence: u64,
    lease: LayoutLeaseToken,
    zone: u64,
    phase: serde_json_core::heapless::String<32>,
    position: LayoutPoint,
    button: Option<u32>,
    #[serde(default)]
    inventory_revision: Option<u64>,
    #[serde(default)]
    base_layout_revision: Option<u64>,
}

#[derive(Clone, Copy, Deserialize)]
struct LayoutPoint {
    u: f64,
    v: f64,
}

#[derive(Clone, Copy, Debug)]
struct ShellDividerDrag {
    interaction: u64,
    zone: u64,
    last_event_sequence: u64,
    pointer_offset_u: f64,
    fixed_edge_px: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct ShellLayoutState {
    sidebar_visible: bool,
    picker_visible: bool,
    keybindings_visible: bool,
    inbox_visible: bool,
    sidebar_width: f64,
    inbox_width: f64,
    focus: Option<u64>,
}

#[derive(Clone, Debug)]
struct PendingShellLayout {
    state: ShellLayoutState,
    native_resize_revision: Option<u64>,
    native_retry_count: u8,
}

#[derive(Clone)]
struct DeferredOverlayLayout {
    state: ShellLayoutState,
    reason: &'static str,
}

#[derive(Deserialize)]
struct LayoutTransactionNotification {
    #[serde(rename = "type")]
    message_type: serde_json_core::heapless::String<32>,
    request_id: u64,
    #[serde(default)]
    reason: Option<serde_json_core::heapless::String<32>>,
    #[serde(default)]
    resize_epoch: Option<LayoutNotificationEpoch>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct LayoutNotificationEpoch {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    lease: u64,
    #[serde(default)]
    lease_generation: u64,
    #[serde(default)]
    base_layout_revision: u64,
    target_layout_revision: u64,
}

#[derive(Deserialize)]
struct LayoutRegionResizeMessage {
    #[serde(rename = "type")]
    message_type: serde_json_core::heapless::String<32>,
    resize_epoch: LayoutNotificationEpoch,
    lease: LayoutLeaseToken,
    size: LayoutStateSize,
    scale: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResizePaintPlan {
    OrdinaryRedraw,
    TaggedRedraw(u64),
    Reuse(u64),
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
    picker: u64,
    result: SessionPickerResult,
}

#[derive(Deserialize)]
struct PickerCreatedMessage {
    #[serde(rename = "type")]
    _message_type: serde_json_core::heapless::String<32>,
    #[serde(default)]
    request: Option<String>,
    picker: u64,
}

#[derive(Deserialize)]
struct PickerCancelledMessage {
    #[serde(rename = "type")]
    _message_type: serde_json_core::heapless::String<32>,
    picker: u64,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<&'static str>,
    title: &'static str,
    placeholder: &'static str,
    items: Vec<SessionPickerItem>,
    loading: bool,
    loading_label: &'static str,
}

#[derive(Serialize)]
struct SessionPickerUpdate {
    #[serde(rename = "type")]
    message_type: &'static str,
    picker: u64,
    items: Vec<SessionPickerItem>,
    loading: bool,
    loading_label: &'static str,
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
struct StatusBarNotice<'a> {
    kind: &'static str,
    message: &'a str,
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
            can_create_container: false,
            pane: 0,
            backend_id: 0,
            session_id: 0,
            sidebar_visible: true,
            width: 320.0,
            height: 720.0,
            resize_extent: (320.0, 720.0, 1.0),
            sessions: Vec::new(),
            backends: Vec::new(),
            collapsed_projects: BTreeSet::new(),
            selected: None,
            query: String::new(),
            mode: EditMode::Search,
            scroll: 0,
            request: 0,
            pending: None,
            pending_directory_backend: None,
            awaiting_backend_catalogue: false,
            backend_picker: None,
            status: String::new(),
            error: false,
            next_coding_agent_ui: 2,
            coding_agent_uis: Vec::new(),
            coding_agent_targets: BTreeMap::new(),
            agents: Vec::new(),
            agent_picker_pending: false,
            diff_viewers: BTreeSet::new(),
            terminal_uis: BTreeMap::new(),
            neovim_uis_starting: BTreeMap::new(),
            neovim_uis: BTreeMap::new(),
            text_cache: None,
            layout: None,
            workspace_focus: None,
            picker_pane: None,
            picker_visible: false,
            keybindings_pane: None,
            keybindings_visible: false,
            inbox_pane: None,
            inbox_visible: false,
            sidebar_width: SIDEBAR_WIDTH,
            inbox_width: INBOX_WIDTH,
            status_bar_pane: None,
            next_layout_request: 0,
            native_resize_revision: None,
            durable_shell_layout: None,
            pending_shell_layouts: BTreeMap::new(),
            deferred_overlay_layout: None,
            pointer_drag: None,
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
                // Session snapshots and backend catalogues can arrive in a
                // different order. Git is a project capability, so one stale
                // or incomplete session row must not hide the worktree action
                // for every other checkout of the project.
                group.git |= session.git;
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
                        start_global_actor(CHILD_ACTOR_ID, COMPONENT_FILE, Vec::new()),
                        start_global_actor(PICKER_ACTOR_ID, PICKER_COMPONENT_FILE, Vec::new()),
                        start_global_actor(
                            KEYBINDINGS_ACTOR_ID,
                            KEYBINDINGS_COMPONENT_FILE,
                            Vec::new(),
                        ),
                        start_global_actor(INBOX_ACTOR_ID, INBOX_COMPONENT_FILE, Vec::new()),
                        start_global_actor(
                            STATUS_BAR_ACTOR_ID,
                            STATUS_BAR_COMPONENT_FILE,
                            Vec::new(),
                        ),
                    ];
                    if let (Some(backend_id), Some(session_id)) =
                        (context.backend_id, context.session_id)
                    {
                        commands.push(start_session_actor(
                            TERMINAL_UI_ID,
                            TERMINAL_UI_COMPONENT_FILE,
                            Vec::new(),
                            backend_id,
                            session_id,
                        ));
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
                    state.inbox_visible = false;
                    state.error = true;
                    state.status = "The global inbox stopped".to_string();
                    return Vec::new();
                }
                if id == STATUS_BAR_ACTOR_ID {
                    state.status_bar_pane = None;
                    state.error = true;
                    state.status = "The status bar stopped".to_string();
                    return Vec::new();
                }
                if id == KEYBINDINGS_ACTOR_ID {
                    state.keybindings_pane = None;
                    state.keybindings_visible = false;
                }
                state.coding_agent_uis.retain(|actor| actor != &id);
                state.coding_agent_targets.remove(&id);
                state.diff_viewers.remove(&id);
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
                    state.inbox_visible = false;
                    state.error = true;
                    state.status = format!("The global inbox failed: {}", failed.message);
                    return status_bar_notice(&state.status).into_iter().collect();
                }
                if failed.id == STATUS_BAR_ACTOR_ID {
                    state.status_bar_pane = None;
                    state.error = true;
                    state.status = format!("The status bar failed: {}", failed.message);
                    return Vec::new();
                }
                if failed.id == KEYBINDINGS_ACTOR_ID {
                    state.keybindings_pane = None;
                    state.keybindings_visible = false;
                }
                state.coding_agent_uis.retain(|actor| actor != &failed.id);
                state.coding_agent_targets.remove(&failed.id);
                state.diff_viewers.remove(&failed.id);
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
                let notice = status_bar_notice(&state.status);
                editor_failure
                    .into_iter()
                    .chain(stop)
                    .chain(notice)
                    .collect()
            }
            Event::Presentation(event) => handle_presentation(event),
            Event::Session(event) => handle_session_event(event),
            Event::Mailbox(message) if message.sender == "layout-host" => {
                handle_layout_message(&message.body)
            }
            Event::Mailbox(message) if message.sender == "pointer-policy" => {
                handle_layout_pointer_message(&message.body)
            }
            Event::Mailbox(message) if message.sender == "key-policy" => {
                handle_key_policy_action(&message.body)
            }
            Event::Mailbox(message) if message.sender == PICKER_ACTOR_ID => {
                handle_picker_message(&message.body)
            }
            Event::Mailbox(message) if message.sender == KEYBINDINGS_ACTOR_ID => {
                handle_keybindings_layout_message(&message.body)
            }
            Event::Mailbox(message) if message.sender == INBOX_ACTOR_ID => {
                handle_inbox_message(&message.body)
            }
            Event::Mailbox(message) if message.sender == STATUS_BAR_ACTOR_ID => {
                handle_status_bar_message(&message.body)
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

fn status_bar_notice(message: &str) -> Option<ActorCommand> {
    serde_json::to_vec(&StatusBarNotice {
        kind: "status-bar-error",
        message,
    })
    .ok()
    .map(|body| {
        ActorCommand::SendLocalMessage(LocalActorMessage {
            actor: STATUS_BAR_ACTOR_ID.to_string(),
            body,
        })
    })
}

fn handle_inbox_message(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((message, consumed)) = serde_json_core::from_slice::<InboxReadyMessage>(bytes) else {
        return Vec::new();
    };
    if consumed != bytes.len() || message.pane == 0 {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match message.kind.as_str() {
        "inbox-ready" => {
            state.inbox_pane = Some(message.pane);
            publish_layout(&state, true, state.workspace_focus)
        }
        "inbox-show" if state.inbox_pane == Some(message.pane) => {
            state.inbox_visible = true;
            publish_synchronized_layout(&mut state, Some(message.pane), "inbox", None)
        }
        _ => Vec::new(),
    }
}

fn handle_status_bar_message(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((message, consumed)) = serde_json_core::from_slice::<InboxReadyMessage>(bytes) else {
        return Vec::new();
    };
    if consumed != bytes.len() || message.kind.as_str() != "status-bar-ready" || message.pane == 0 {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    state.status_bar_pane = Some(message.pane);
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

fn apply_backend_sessions(
    state: &mut SwitcherState,
    can_create_local: bool,
    can_connect_remote: bool,
    can_create_container: bool,
    backends: Vec<BackendCatalogueEntry>,
) {
    state.can_create_local = can_create_local;
    state.can_connect_remote = can_connect_remote;
    state.can_create_container = can_create_container;
    state.backends = backends
        .iter()
        .map(|backend| (backend.backend.0, backend.label.clone()))
        .collect();
    state.backends.sort_by_key(|(backend, _)| *backend);
    let current_backend = state.backend_id;
    let current_sessions = state
        .sessions
        .iter()
        .filter(|session| session.backend == current_backend)
        .cloned()
        .collect::<Vec<_>>();
    let mut catalogue_sessions = backends
        .into_iter()
        .flat_map(|backend| {
            backend
                .sessions
                .into_iter()
                .map(move |session| ViewSession {
                    backend: backend.backend.0,
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
        .collect::<Vec<_>>();
    // The global catalogue is discovered asynchronously and can initially
    // contain an empty row for the active backend even though that backend's
    // live session snapshot has already arrived. Preserve that newer snapshot
    // until the catalogue has real rows to replace it. A genuine empty state
    // still arrives through `apply_snapshot`, which removes the sessions.
    if current_backend != 0
        && !current_sessions.is_empty()
        && !catalogue_sessions
            .iter()
            .any(|session| session.backend == current_backend)
    {
        catalogue_sessions.extend(current_sessions);
    }
    state.sessions = catalogue_sessions;
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
    let backend = backend_from_sender(sender).unwrap_or(state.backend_id);
    apply_worktree_completion(
        &mut state,
        backend,
        completion.request,
        completion.success,
        completion.session,
        completion.message.as_str(),
    );
    redraw_or_record_error(&mut state);
    Vec::new()
}

fn apply_worktree_completion(
    state: &mut SwitcherState,
    backend: u64,
    request: u64,
    success: bool,
    session: Option<u64>,
    message: &str,
) {
    if state.pending != Some(request) {
        return;
    }
    state.pending = None;
    state.error = !success;
    if success {
        if let Some(session) = session {
            // The completion and catalogue use independent bounded lanes. Keep
            // the authoritative identity even when completion wins the race;
            // the following catalogue update will retain this selection once
            // it adds the row.
            state.backend_id = backend;
            state.session_id = session;
            state.selected = Some((backend, session));
        }
        state.status = "Created worktree session".to_string();
        state.query.clear();
        state.mode = EditMode::Search;
    } else {
        state.status = if message.trim().is_empty() {
            "Worktree creation failed: the backend returned no error details".to_string()
        } else {
            format!("Worktree creation failed: {}", message.trim())
        };
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
            state.resize_extent = (created.width, created.height, created.scale);
            redraw_or_record_error(&mut state);
            vec![ActorCommand::SendHostMessage(
                br#"{"type":"claim-root-layout"}"#.to_vec(),
            )]
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            let width = resized.width.max(240.0);
            let height = resized.height.max(240.0);
            let target_extent = (resized.width, resized.height, resized.scale);
            let paint = resize_paint_plan(state.resize_extent, target_extent, resized.resize_epoch);
            state.width = width;
            state.height = height;
            state.resize_extent = target_extent;
            state.hovered = None;
            state.pressed = None;
            state.ensure_selected_visible();
            paint_for_resize_or_record_error(&mut state, paint);
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
    if let Some(notification) = decode_layout_transaction_notification(bytes) {
        let Ok(mut state) = STATE.lock() else {
            return Vec::new();
        };
        return apply_layout_transaction_notification(&mut state, &notification);
    }
    if let Ok((message, consumed)) = serde_json_core::from_slice::<LayoutRegionResizeMessage>(bytes)
        && consumed == bytes.len()
        && message.message_type.as_str() == "layout-region-resized"
    {
        return handle_layout_region_resize(message);
    }
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
    if message.focused_pane != Some(state.pane)
        && message.focused_pane != state.picker_pane
        && message.focused_pane != state.keybindings_pane
    {
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
    let requested_focus = shell_requested_focus(&state, message.focused_pane);
    let native_resize_revision = message
        .native_resize_revision
        .filter(|revision| *revision != 0 && Some(*revision) != state.native_resize_revision);
    state.layout = Some(LayoutContext {
        revision: message.revision,
        presented_layout_revision: message.presented_layout_revision.unwrap_or(0),
        lease: message.lease,
        waitable_panes: message.waitable_panes,
        presented_panes: message.presented_panes,
        visible_panes: message.visible_panes,
        focused_pane: requested_focus,
        size: message.size,
        rest: message.rest,
    });
    if state.pending_shell_layouts.is_empty() {
        state.durable_shell_layout = Some(shell_layout_state(&state, requested_focus));
    }
    let mut commands = if let Some(native_resize_revision) = native_resize_revision {
        let commands = publish_synchronized_layout(
            &mut state,
            requested_focus,
            "native",
            Some(native_resize_revision),
        );
        if !commands.is_empty() {
            state.native_resize_revision = Some(native_resize_revision);
        }
        commands
    } else {
        publish_layout(&state, true, requested_focus)
    };
    if realm_changed {
        commands.push(subscribe_agent_registry(state.backend_id, state.session_id));
    }
    commands
}

fn handle_layout_region_resize(message: LayoutRegionResizeMessage) -> Vec<ActorCommand> {
    if !message.scale.is_finite() || message.scale <= 0.0 {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    publish_layout_region_response(&mut state, &message)
}

fn publish_layout_region_response(
    state: &mut SwitcherState,
    message: &LayoutRegionResizeMessage,
) -> Vec<ActorCommand> {
    let Some(context) = state.layout.as_ref() else {
        return Vec::new();
    };
    if context.lease != message.lease {
        return Vec::new();
    }
    let revision = context.revision;
    let focus = context.focused_pane;
    let old_size = context.size;
    if let Some(context) = state.layout.as_mut() {
        context.size = message.size;
    }
    let publication = resolve_shell_layout(state, false, focus);
    if let Some(context) = state.layout.as_mut() {
        context.size = old_size;
    }
    let Some(publication) = publication else {
        return vec![layout_region_failure_command(
            message,
            "the shell fragment is not available",
        )];
    };
    serde_json::to_vec(&SubmitLayoutRegion {
        message_type: "submit-layout-region",
        resize_epoch: message.resize_epoch,
        lease: publication.lease,
        inventory_revision: revision,
        fragment: &publication.fragment,
        focus,
    })
    .ok()
    .map(ActorCommand::SendHostMessage)
    .into_iter()
    .collect()
}

fn layout_region_failure_command(
    message: &LayoutRegionResizeMessage,
    reason: &'static str,
) -> ActorCommand {
    ActorCommand::SendHostMessage(
        serde_json::to_vec(&serde_json::json!({
            "type": "fail-layout-region",
            "resize_epoch": message.resize_epoch,
            "lease": message.lease,
            "message": reason,
        }))
        .unwrap_or_default(),
    )
}

fn handle_layout_pointer_message(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((message, consumed)) = serde_json_core::from_slice::<LayoutPointerMessage>(bytes) else {
        return Vec::new();
    };
    if consumed != bytes.len()
        || message.message_type.as_str() != "layout-pointer"
        || !message.position.u.is_finite()
        || !message.position.v.is_finite()
        || !matches!(
            message.zone,
            SIDEBAR_DIVIDER_ZONE_ID | INBOX_DIVIDER_ZONE_ID
        )
    {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    apply_layout_pointer_message(&mut state, &message)
}

fn apply_layout_pointer_message(
    state: &mut SwitcherState,
    message: &LayoutPointerMessage,
) -> Vec<ActorCommand> {
    let Some(interaction) = message.interaction.filter(|interaction| *interaction != 0) else {
        return Vec::new();
    };
    let pane = state.pane;
    let requested_sidebar_visible = state.sidebar_visible;
    let requested_inbox_visible = state.inbox_visible;
    let inbox_exists = state.inbox_pane.is_some();
    let Some(layout) = state.layout.as_mut() else {
        return Vec::new();
    };
    if layout.lease != message.lease || !layout.rest {
        return Vec::new();
    }
    if let Some(inventory_revision) = message.inventory_revision {
        layout.revision = inventory_revision;
    }
    if let Some(base_layout_revision) = message.base_layout_revision {
        layout.presented_layout_revision = base_layout_revision;
    }
    let width = f64::from(layout.size.w.max(1));
    let sidebar_visible = requested_sidebar_visible && layout.visible_panes.contains(&pane);
    let inbox_visible = requested_inbox_visible && inbox_exists;
    if (message.zone == SIDEBAR_DIVIDER_ZONE_ID && !sidebar_visible)
        || (message.zone == INBOX_DIVIDER_ZONE_ID && !inbox_visible)
    {
        return Vec::new();
    }
    let columns = shell_column_geometry(
        width,
        true,
        sidebar_visible,
        inbox_visible,
        state.sidebar_width,
        state.inbox_width,
    );

    if message.phase.as_str() == "button-press" && message.button == Some(PRIMARY_BUTTON) {
        let boundary_px = if message.zone == SIDEBAR_DIVIDER_ZONE_ID {
            columns.rest_width
        } else {
            width - columns.inbox_width
        };
        let boundary_u = boundary_px * 2.0 / width - 1.0;
        state.pointer_drag = Some(ShellDividerDrag {
            interaction,
            zone: message.zone,
            last_event_sequence: message.event_sequence,
            pointer_offset_u: boundary_u - message.position.u,
            fixed_edge_px: if message.zone == SIDEBAR_DIVIDER_ZONE_ID {
                columns.rest_width + columns.sidebar_width
            } else {
                columns.rest_width
            },
        });
        return Vec::new();
    }

    let Some(drag) = state.pointer_drag else {
        return Vec::new();
    };
    if drag.interaction != interaction
        || drag.zone != message.zone
        || message.event_sequence <= drag.last_event_sequence
    {
        return Vec::new();
    }
    if message.phase.as_str() == "cancel" {
        state.pointer_drag = None;
        return Vec::new();
    }
    if message.phase.as_str() != "motion"
        && !(message.phase.as_str() == "button-release" && message.button == Some(PRIMARY_BUTTON))
    {
        return Vec::new();
    }

    let requested_u = message.position.u + drag.pointer_offset_u;
    let requested_boundary = ((requested_u + 1.0) * 0.5 * width).clamp(0.0, width);
    if message.zone == SIDEBAR_DIVIDER_ZONE_ID {
        let minimum_main = MIN_MAIN_AREA_WIDTH.min(drag.fixed_edge_px * 0.5);
        let minimum_sidebar =
            MIN_SHELL_PANE_WIDTH.min((drag.fixed_edge_px - minimum_main).max(0.0));
        let boundary = requested_boundary.clamp(
            minimum_main,
            (drag.fixed_edge_px - minimum_sidebar).max(minimum_main),
        );
        state.sidebar_width = drag.fixed_edge_px - boundary;
    } else if sidebar_visible {
        let available = (width - drag.fixed_edge_px).max(0.0);
        let minimum_shell = MIN_SHELL_PANE_WIDTH.min(available * 0.5);
        let boundary =
            requested_boundary.clamp(drag.fixed_edge_px + minimum_shell, width - minimum_shell);
        state.sidebar_width = boundary - drag.fixed_edge_px;
        state.inbox_width = width - boundary;
    } else {
        let minimum_main = MIN_MAIN_AREA_WIDTH.min(width * 0.5);
        let minimum_inbox = MIN_SHELL_PANE_WIDTH.min((width - minimum_main).max(0.0));
        let boundary = requested_boundary.clamp(minimum_main, width - minimum_inbox);
        state.inbox_width = width - boundary;
    }
    if let Some(active) = state.pointer_drag.as_mut() {
        active.last_event_sequence = message.event_sequence;
    }
    let reason = if message.zone == SIDEBAR_DIVIDER_ZONE_ID {
        "sidebar-resize"
    } else {
        "inbox-resize"
    };
    let commands =
        publish_synchronized_pointer_layout(state, interaction, message.event_sequence, reason);
    if commands.is_empty() {
        restore_latest_submitted_or_durable_shell_layout(state);
    }
    if message.phase.as_str() == "button-release" {
        state.pointer_drag = None;
    }
    commands
}

fn decode_layout_transaction_notification(bytes: &[u8]) -> Option<LayoutTransactionNotification> {
    let (notification, consumed) =
        serde_json_core::from_slice::<LayoutTransactionNotification>(bytes).ok()?;
    (consumed == bytes.len()
        && matches!(
            notification.message_type.as_str(),
            "accepted" | "completed" | "degraded" | "rejected" | "aborted" | "superseded"
        ))
    .then_some(notification)
}

fn shell_layout_state(state: &SwitcherState, focus: Option<u64>) -> ShellLayoutState {
    ShellLayoutState {
        sidebar_visible: state.sidebar_visible,
        picker_visible: state.picker_visible,
        keybindings_visible: state.keybindings_visible,
        inbox_visible: state.inbox_visible,
        sidebar_width: state.sidebar_width,
        inbox_width: state.inbox_width,
        focus,
    }
}

fn restore_shell_layout_state(state: &mut SwitcherState, shell: &ShellLayoutState) {
    state.sidebar_visible = shell.sidebar_visible;
    state.picker_visible = shell.picker_visible;
    state.keybindings_visible = shell.keybindings_visible;
    state.inbox_visible = shell.inbox_visible;
    state.sidebar_width = shell.sidebar_width;
    state.inbox_width = shell.inbox_width;
    if let Some(layout) = state.layout.as_mut() {
        layout.focused_pane = shell.focus;
    }
}

fn restore_latest_submitted_or_durable_shell_layout(state: &mut SwitcherState) {
    let restore = state
        .pending_shell_layouts
        .last_key_value()
        .map(|(_request, pending)| pending.state.clone())
        .or_else(|| state.durable_shell_layout.clone());
    if let Some(restore) = restore {
        restore_shell_layout_state(state, &restore);
    }
}

fn apply_layout_transaction_notification(
    state: &mut SwitcherState,
    notification: &LayoutTransactionNotification,
) -> Vec<ActorCommand> {
    match notification.message_type.as_str() {
        "completed" | "degraded" => {
            if let Some(completed) = state.pending_shell_layouts.remove(&notification.request_id) {
                state.durable_shell_layout = Some(completed.state);
            }
            if let Some(target_layout_revision) = notification
                .resize_epoch
                .map(|epoch| epoch.target_layout_revision)
                && let Some(layout) = state.layout.as_mut()
            {
                layout.presented_layout_revision = target_layout_revision;
            }
            if state.pending_shell_layouts.is_empty()
                && let Some(deferred) = state.deferred_overlay_layout.take()
            {
                let focus = deferred.state.focus;
                restore_shell_layout_state(state, &deferred.state);
                return publish_synchronized_layout_attempt(
                    state,
                    focus,
                    deferred.reason,
                    None,
                    0,
                    None,
                    None,
                );
            }
        }
        "rejected" | "aborted" => {
            let rejected = state.pending_shell_layouts.remove(&notification.request_id);
            if notification.message_type.as_str() == "rejected"
                && notification.reason.as_ref().map(|reason| reason.as_str())
                    == Some("stale-inventory-revision")
                && let Some(rejected) = rejected.as_ref()
                && rejected.native_resize_revision.is_none()
            {
                // A pane was added while an ordinary shell transition was in
                // flight. The host rejected only the stale inventory token;
                // the user's requested picker/sidebar state is still valid.
                // Preserve it so the already-queued fresh layout-state can
                // republish against the new inventory instead of resurrecting
                // the previous durable overlay and stealing focus.
                restore_shell_layout_state(state, &rejected.state);
                return Vec::new();
            }
            restore_latest_submitted_or_durable_shell_layout(state);
            if let Some(rejected) = rejected
                && let Some(native_resize_revision) = rejected.native_resize_revision
                && state.native_resize_revision == Some(native_resize_revision)
            {
                if notification.message_type.as_str() == "rejected"
                    && notification.reason.as_ref().map(|reason| reason.as_str()) == Some("busy")
                    && rejected.native_retry_count < MAX_NATIVE_RESIZE_RETRIES
                {
                    let focus = rejected.state.focus;
                    return publish_synchronized_layout_attempt(
                        state,
                        focus,
                        "native",
                        Some(native_resize_revision),
                        rejected.native_retry_count + 1,
                        None,
                        None,
                    );
                }
                if notification.message_type.as_str() == "rejected"
                    && rejected.native_retry_count < MAX_NATIVE_RESIZE_RETRIES
                {
                    // A stale or invalid request did not consume the host's
                    // next publication. If the same nonce is legitimately
                    // republished, allow the layout-state path to submit it.
                    state.native_resize_revision = None;
                }
            }
        }
        "superseded" => {
            state.pending_shell_layouts.remove(&notification.request_id);
        }
        "accepted" => {}
        _ => unreachable!("notification kind was validated before dispatch"),
    }
    Vec::new()
}

fn shell_requested_focus(state: &SwitcherState, fallback: Option<u64>) -> Option<u64> {
    state
        .keybindings_visible
        .then_some(state.keybindings_pane)
        .flatten()
        .or_else(|| state.picker_visible.then_some(state.picker_pane).flatten())
        .or(fallback)
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
        return toggle_diff(&mut state, backend_id, session_id);
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
        "new" => {
            return request_creation_backend_picker(&mut state);
        }
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
    publish_synchronized_layout(&mut state, requested_focus, "sidebar", None)
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
    if let Ok(message) = serde_json::from_slice::<BackendCatalogueMessage>(bytes)
        && message.validate().is_ok()
    {
        match message {
            BackendCatalogueMessage::ConnectionFailed { message, .. } => {
                if let Ok(mut state) = STATE.lock() {
                    state.mode = EditMode::Search;
                    state.pending_directory_backend = None;
                    state.error = true;
                    state.status = message;
                    redraw_or_record_error(&mut state);
                }
            }
            BackendCatalogueMessage::Connected { backend, .. } => {
                if let Ok(mut state) = STATE.lock() {
                    state.pending_directory_backend = Some(backend.0);
                    state.status = format!("Waiting for backend {} inventory…", backend.0);
                    redraw_or_record_error(&mut state);
                }
            }
            BackendCatalogueMessage::Sessions {
                can_create_local,
                can_connect_remote,
                can_create_container,
                backends,
                ..
            } => {
                if let Ok(mut state) = STATE.lock() {
                    apply_backend_sessions(
                        &mut state,
                        can_create_local,
                        can_connect_remote,
                        can_create_container,
                        backends,
                    );
                    if state.awaiting_backend_catalogue {
                        state.awaiting_backend_catalogue = false;
                        state.status = "Backends discovered".to_string();
                        state.error = false;
                        if let Some(update) = update_creation_backend_picker(&state) {
                            return vec![update];
                        }
                    }
                    if let Some(backend) = state.pending_directory_backend
                        && state
                            .backends
                            .iter()
                            .any(|(candidate, _)| *candidate == backend)
                    {
                        return open_directory_picker(&mut state, backend);
                    }
                    redraw_or_record_error(&mut state);
                }
            }
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
            // When the shortcut had to wait for the session's first agent,
            // there is no choice to present. Open that agent directly rather
            // than flashing an empty/stale picker after its creation.
            if state.agents.len() == 1 {
                state.agent_picker_pending = false;
                let agent = state.agents[0].id.clone();
                return open_coding_agent_ui(&mut state, agent);
            }
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
    vec![start_session_actor(
        id,
        TERMINAL_UI_COMPONENT_FILE,
        Vec::new(),
        backend_id,
        session_id,
    )]
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
                "diff" => toggle_diff(&mut state, backend_id, session_id),
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
    start_session_actor(
        id,
        NEOVIM_CLIENT_COMPONENT_FILE,
        vec![
            "runtime:package-cwd".to_string(),
            "subprocess:node".to_string(),
        ],
        backend_id,
        session_id,
    )
}

impl EditorMessage {
    fn from_bytes(bytes: &[u8]) -> Self {
        serde_json::from_slice(bytes).unwrap_or_default()
    }
}

fn sender_actor_is(sender: &str, expected: &str) -> bool {
    sender.rsplit(':').next() == Some(expected)
}

fn toggle_diff(state: &mut SwitcherState, backend_id: u64, session_id: u64) -> Vec<ActorCommand> {
    let id = format!("{DIFF_VIEWER_ID_PREFIX}b{backend_id}-s{session_id}");
    if state.diff_viewers.remove(&id) {
        vec![ActorCommand::StopActor(id)]
    } else {
        state.diff_viewers.insert(id.clone());
        vec![start_diff_viewer(&id, backend_id, session_id)]
    }
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
            let Ok((message, consumed)) =
                serde_json_core::from_slice::<PickerCreatedMessage>(bytes)
            else {
                return Vec::new();
            };
            if consumed != bytes.len() {
                return Vec::new();
            }
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            if message.request.as_deref() == Some(BACKEND_PICKER_REQUEST) {
                state.backend_picker = Some(message.picker);
                state.status = if state.awaiting_backend_catalogue {
                    "Discovering backends…".to_string()
                } else {
                    "Backends discovered".to_string()
                };
                state.error = false;
                redraw_or_record_error(&mut state);
                if !state.awaiting_backend_catalogue
                    && let Some(update) = update_creation_backend_picker(&state)
                {
                    return vec![update];
                }
                return Vec::new();
            }
            state.status = "Session picker opened".to_string();
            state.error = false;
            redraw_or_record_error(&mut state);
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
        "picker-cancelled" => {
            if let Ok((message, consumed)) =
                serde_json_core::from_slice::<PickerCancelledMessage>(bytes)
                && consumed == bytes.len()
                && let Ok(mut state) = STATE.lock()
                && state.backend_picker == Some(message.picker)
            {
                state.backend_picker = None;
                state.awaiting_backend_catalogue = false;
            }
            Vec::new()
        }
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
    let editing_in_switcher = matches!(
        state.mode,
        EditMode::Rename(..)
            | EditMode::CreateWorktree(..)
            | EditMode::ConnectRemote
            | EditMode::CreateLocalContainer
            | EditMode::CreateRemoteContainer
    );
    let focus = if message.visible {
        Some(message.pane)
    } else if editing_in_switcher {
        Some(state.pane)
    } else {
        state
            .workspace_focus
            .or_else(|| state.sidebar_visible.then_some(state.pane))
    };
    if let Some(layout) = state.layout.as_mut() {
        layout.focused_pane = focus;
    }
    publish_synchronized_layout(&mut state, focus, "picker", None)
}

fn handle_keybindings_layout_message(bytes: &[u8]) -> Vec<ActorCommand> {
    let Ok((message, consumed)) = serde_json_core::from_slice::<PickerLayoutMessage>(bytes) else {
        return Vec::new();
    };
    if consumed != bytes.len() || message.message_type.as_str() != "keybindings-pane" {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if message.pane == 0 {
        return Vec::new();
    }
    state.keybindings_pane = Some(message.pane);
    state.keybindings_visible = message.visible;
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
    publish_synchronized_layout(&mut state, focus, "keybindings", None)
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
    if state.backend_picker == Some(message.picker) {
        state.backend_picker = None;
        state.awaiting_backend_catalogue = false;
    }
    if let Some(action) = message.result.value.action {
        match action.as_str() {
            "create-worktree" => {
                let Some(backend) = message.result.value.backend else {
                    return Vec::new();
                };
                let Some(source_session) = message.result.value.session else {
                    return Vec::new();
                };
                if !begin_worktree_for_source(&mut state, backend, source_session) {
                    state.error = true;
                    state.status = "The selected Git project is no longer available".to_string();
                }
                redraw_or_record_error(&mut state);
                return Vec::new();
            }
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
            "create-container-local" => {
                state.query.clear();
                state.mode = EditMode::CreateLocalContainer;
                state.status = "OCI image: Enter to create and connect".to_string();
                redraw_or_record_error(&mut state);
                return Vec::new();
            }
            "create-container-remote" => {
                state.query.clear();
                state.mode = EditMode::CreateRemoteContainer;
                state.status = "Podman connection::OCI image".to_string();
                redraw_or_record_error(&mut state);
                return Vec::new();
            }
            _ => {}
        }
    }
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
    let Some(agent) = message.result.value.agent else {
        return Vec::new();
    };
    if !state.agents.iter().any(|available| available.id == agent) {
        state.error = true;
        state.status = "The selected agent is no longer available".to_string();
        return Vec::new();
    }
    open_coding_agent_ui(&mut state, agent)
}

fn open_coding_agent_ui(state: &mut SwitcherState, agent: String) -> Vec<ActorCommand> {
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
        request: None,
        title: "Switch session",
        placeholder: "Type to fuzzy-match sessions…",
        items,
        loading: false,
        loading_label: "",
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

fn creation_backend_items(state: &SwitcherState) -> Vec<SessionPickerItem> {
    let mut picker_items = Vec::new();
    for project in state
        .project_groups()
        .into_iter()
        .filter(|project| project.git)
    {
        let source = state.sessions.iter().find(|session| {
            session.backend == project.backend && session.id == project.source_session
        });
        let source_label = source
            .and_then(|session| session.branch.as_deref())
            .unwrap_or("the current checkout");
        picker_items.push(SessionPickerItem {
            label: format!("Worktree · {}", project.name),
            detail: format!("Create a new branch from {source_label}"),
            value: SessionPickerValueWire {
                backend: Some(project.backend),
                session: Some(project.source_session),
                action: Some("create-worktree".to_string()),
                directory: None,
            },
        });
    }
    for (backend, backend_label) in &state.backends {
        let label = if backend_label.is_empty() {
            format!("Backend {backend}")
        } else {
            backend_label.clone()
        };
        picker_items.push(SessionPickerItem {
            label,
            detail: "Create a session on this backend".to_string(),
            value: SessionPickerValueWire {
                backend: Some(*backend),
                session: None,
                action: Some("choose-directory".to_string()),
                directory: None,
            },
        });
    }
    if state.can_create_local {
        picker_items.push(SessionPickerItem {
            label: "Local".to_string(),
            detail: "Start a local backend, then create a session".to_string(),
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
    if state.can_create_container {
        picker_items.push(SessionPickerItem {
            label: "Create container backend".to_string(),
            detail: "Run a backend in a local Podman container with GPU auto-detection".to_string(),
            value: SessionPickerValueWire {
                backend: None,
                session: None,
                action: Some("create-container-local".to_string()),
                directory: None,
            },
        });
        picker_items.push(SessionPickerItem {
            label: "Create remote container backend".to_string(),
            detail: "Use a named remote Podman system connection".to_string(),
            value: SessionPickerValueWire {
                backend: None,
                session: None,
                action: Some("create-container-remote".to_string()),
                directory: None,
            },
        });
    }
    picker_items
}

fn creation_backend_picker_command(
    state: &mut SwitcherState,
    loading: bool,
) -> Option<ActorCommand> {
    let request = SessionPickerRequest {
        message_type: "picker-create",
        request: loading.then_some(BACKEND_PICKER_REQUEST),
        title: "Create session",
        placeholder: "Choose a worktree, directory, or backend…",
        items: creation_backend_items(state),
        loading,
        loading_label: if loading {
            "Discovering backends…"
        } else {
            ""
        },
    };
    let Ok(body) = serde_json::to_vec(&request) else {
        state.error = true;
        state.status = "Could not encode the backend picker".to_string();
        redraw_or_record_error(state);
        return None;
    };
    Some(ActorCommand::SendLocalMessage(LocalActorMessage {
        actor: PICKER_ACTOR_ID.to_string(),
        body,
    }))
}

#[cfg(test)]
fn open_creation_backend_picker(state: &mut SwitcherState) -> Vec<ActorCommand> {
    creation_backend_picker_command(state, false)
        .into_iter()
        .collect()
}

fn update_creation_backend_picker(state: &SwitcherState) -> Option<ActorCommand> {
    let picker = state.backend_picker?;
    let body = serde_json::to_vec(&SessionPickerUpdate {
        message_type: "picker-update",
        picker,
        items: creation_backend_items(state),
        loading: false,
        loading_label: "",
    })
    .ok()?;
    Some(ActorCommand::SendLocalMessage(LocalActorMessage {
        actor: PICKER_ACTOR_ID.to_string(),
        body,
    }))
}

fn request_creation_backend_picker(state: &mut SwitcherState) -> Vec<ActorCommand> {
    state.awaiting_backend_catalogue = true;
    state.backend_picker = None;
    state.status = "Discovering backends…".to_string();
    redraw_or_record_error(state);
    let mut commands = creation_backend_picker_command(state, true)
        .into_iter()
        .collect::<Vec<_>>();
    commands.push(ActorCommand::SendHostMessage(
        br#"{"type":"request-backend-catalogue"}"#.to_vec(),
    ));
    commands
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
        request: None,
        title: "Choose directory",
        placeholder: "Fuzzy-match a directory…",
        items,
        loading: false,
        loading_label: "",
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
    start_session_actor(
        id,
        CODING_AGENT_COMPONENT_FILE,
        Vec::new(),
        backend_id,
        session_id,
    )
}

fn start_diff_viewer(id: &str, backend_id: u64, session_id: u64) -> ActorCommand {
    start_session_actor(
        id,
        DIFF_VIEWER_COMPONENT_FILE,
        vec![
            "filesystem:workspace-read".to_string(),
            "subprocess:git".to_string(),
        ],
        backend_id,
        session_id,
    )
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
    Some(start_session_actor(
        id,
        TERMINAL_UI_COMPONENT_FILE,
        Vec::new(),
        backend_id,
        session_id,
    ))
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

#[derive(Clone, Copy, Debug)]
struct ShellColumnGeometry {
    width: f64,
    rest_width: f64,
    sidebar_width: f64,
    inbox_width: f64,
}

impl ShellColumnGeometry {
    fn rest_fraction(self) -> f64 {
        self.rest_width / self.width
    }

    fn inbox_fraction(self) -> f64 {
        self.inbox_width / self.width
    }
}

fn shell_column_geometry(
    width: f64,
    has_rest: bool,
    sidebar_visible: bool,
    inbox_visible: bool,
    requested_sidebar_width: f64,
    requested_inbox_width: f64,
) -> ShellColumnGeometry {
    let width = width.max(1.0);
    let visible_shell_panes = usize::from(sidebar_visible) + usize::from(inbox_visible);
    let minimum_main = if has_rest {
        MIN_MAIN_AREA_WIDTH.min(width / (visible_shell_panes as f64 + 1.0))
    } else {
        0.0
    };
    let minimum_shell = if visible_shell_panes > 0 {
        MIN_SHELL_PANE_WIDTH.min((width - minimum_main).max(0.0) / visible_shell_panes as f64)
    } else {
        0.0
    };
    let maximum_shell_total = (width - minimum_main).max(0.0);
    let mut sidebar_width = if sidebar_visible {
        requested_sidebar_width
            .max(minimum_shell)
            .min(maximum_shell_total)
    } else {
        0.0
    };
    let mut inbox_width = if inbox_visible {
        requested_inbox_width
            .max(minimum_shell)
            .min(maximum_shell_total)
    } else {
        0.0
    };
    let excess = (sidebar_width + inbox_width - maximum_shell_total).max(0.0);
    if excess > 0.0 {
        let adjustable_sidebar = (sidebar_width - minimum_shell).max(0.0);
        let adjustable_inbox = (inbox_width - minimum_shell).max(0.0);
        let adjustable = adjustable_sidebar + adjustable_inbox;
        if adjustable > 0.0 {
            sidebar_width -= excess * adjustable_sidebar / adjustable;
            inbox_width -= excess * adjustable_inbox / adjustable;
        }
    }
    let rest_width = (width - sidebar_width - inbox_width).max(0.0);
    ShellColumnGeometry {
        width,
        rest_width,
        sidebar_width,
        inbox_width,
    }
}

fn divider_item(id: u64, zone: u64, center: f64, width: f64, content_size: f64) -> DividerItem {
    let divider_width = (2.0 / width).clamp(0.001, 0.02);
    DividerItem {
        id,
        zone,
        z: 81,
        transform: Transform::new(
            center - divider_width * 0.5,
            -1.0,
            divider_width,
            content_size,
        ),
        axis: "vertical",
        role: "split",
    }
}

fn divider_zone(id: u64, center: f64, width: f64, content_size: f64) -> InputZone {
    let zone_width = (16.0 / width).clamp(0.005, 0.05);
    InputZone {
        id,
        transform: Transform::new(center - zone_width * 0.5, -1.0, zone_width, content_size),
        z: 80,
        event_mask: DIVIDER_EVENT_MASK,
        cursor: "col-resize",
        input: "drag",
    }
}

struct ShellPublication {
    lease: Lease,
    revision: u64,
    delegated: Vec<u64>,
    needs_rest_delegation: bool,
    fragment: LayoutFragment,
    focus: Option<u64>,
}

fn resolve_shell_layout(
    state: &SwitcherState,
    allow_delegation: bool,
    requested_focus: Option<u64>,
) -> Option<ShellPublication> {
    let message = state.layout.as_ref()?;
    // The global shell actors are started together, but their pane-created
    // messages race the first root layout-state publication.  Publishing
    // before every shell pane is known can delegate a late pane to the
    // workspace lease.  The root fragment then cannot legally compose that
    // pane when its ready message arrives.  Hold the initial composition until
    // the complete shell catalogue is available so chrome ownership is stable
    // from the first delegation onward.
    if state.picker_pane.is_none()
        || state.keybindings_pane.is_none()
        || state.inbox_pane.is_none()
        || state.status_bar_pane.is_none()
    {
        return None;
    }
    let pane = state.pane;
    let picker_pane = state.picker_pane;
    let keybindings_pane = state.keybindings_pane;
    let inbox_pane = state.inbox_pane;
    let status_bar_pane = state.status_bar_pane;
    let delegated = message
        .visible_panes
        .iter()
        .copied()
        .filter(|visible| {
            *visible != pane
                && Some(*visible) != picker_pane
                && Some(*visible) != keybindings_pane
                && Some(*visible) != inbox_pane
                && Some(*visible) != status_bar_pane
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
    let keybindings_visible = state.keybindings_visible
        && keybindings_pane.is_some_and(|keybindings| message.visible_panes.contains(&keybindings));
    let inbox_visible = state.inbox_visible && inbox_pane.is_some();
    // Shell chrome is introduced by this root fragment, so it cannot depend
    // on already being present in the previously accepted visible set.
    let status_bar_visible = status_bar_pane.is_some();
    let width = f64::from(message.size.w.max(1));
    let height = f64::from(message.size.height.max(1));
    let columns = shell_column_geometry(
        width,
        has_rest,
        pane_visible,
        inbox_visible,
        state.sidebar_width,
        state.inbox_width,
    );
    let inbox_fraction = columns.inbox_fraction();
    let rest_fraction = columns.rest_fraction();
    let sidebar_offset = -1.0 + 2.0 * rest_fraction;
    let inbox_offset = 1.0 - 2.0 * inbox_fraction;
    // Derive edge-touching extents from their endpoints.  Computing an offset
    // and its extent independently can serialize a mathematically exact edge
    // as a value just beyond 1.0, which the host correctly rejects.  Keeping
    // one shared boundary also prevents gaps between the shell columns.
    let sidebar_end = if inbox_visible { inbox_offset } else { 1.0 };
    let sidebar_size = sidebar_end - sidebar_offset;
    let inbox_size = 1.0 - inbox_offset;
    let rest_size = sidebar_offset + 1.0;
    let status_fraction = if status_bar_visible {
        (STATUS_BAR_HEIGHT / height).clamp(0.03, 0.14)
    } else {
        0.0
    };
    let status_offset = 1.0 - 2.0 * status_fraction;
    let content_size = status_offset + 1.0;
    let status_size = 1.0 - status_offset;

    let mut layout_items = Vec::new();
    if pane_visible {
        layout_items.push(LayoutItem {
            id: pane,
            kind: ItemKind::Pane { pane },
            z: 1,
            transform: Transform::new(sidebar_offset, -1.0, sidebar_size, content_size),
            input: "pane",
        });
    }
    if inbox_visible && let Some(inbox) = inbox_pane {
        layout_items.push(LayoutItem {
            id: inbox,
            kind: ItemKind::Pane { pane: inbox },
            z: 2,
            transform: Transform::new(inbox_offset, -1.0, inbox_size, content_size),
            input: "pane",
        });
    }
    if status_bar_visible && let Some(status) = status_bar_pane {
        layout_items.push(LayoutItem {
            id: status,
            kind: ItemKind::Pane { pane: status },
            z: 90,
            transform: Transform::new(-1.0, status_offset, 2.0, status_size),
            input: "pane",
        });
    }
    if picker_visible && let Some(picker) = picker_pane {
        let picker_width = (720.0 / width).clamp(0.45, 0.88);
        let picker_height = (480.0 / height).clamp(0.40, 0.82);
        layout_items.push(LayoutItem {
            id: picker,
            kind: ItemKind::OverlayPane { pane: picker },
            z: 100,
            transform: Transform::new(
                -picker_width,
                -picker_height,
                picker_width * 2.0,
                picker_height * 2.0,
            ),
            input: "pane",
        });
    }
    if keybindings_visible && let Some(keybindings) = keybindings_pane {
        layout_items.push(LayoutItem {
            id: keybindings,
            kind: ItemKind::OverlayPane { pane: keybindings },
            z: 110,
            transform: Transform::new(-1.0, -1.0, 2.0, 2.0),
            input: "pane",
        });
    }
    let mut zones = Vec::new();
    let mut dividers = Vec::new();
    if has_rest && pane_visible {
        let center = -1.0 + 2.0 * rest_fraction;
        zones.push(divider_zone(
            SIDEBAR_DIVIDER_ZONE_ID,
            center,
            width,
            content_size,
        ));
        dividers.push(divider_item(
            SIDEBAR_DIVIDER_ITEM_ID,
            SIDEBAR_DIVIDER_ZONE_ID,
            center,
            width,
            content_size,
        ));
    }
    if has_rest && inbox_visible {
        let center = 1.0 - 2.0 * inbox_fraction;
        zones.push(divider_zone(
            INBOX_DIVIDER_ZONE_ID,
            center,
            width,
            content_size,
        ));
        dividers.push(divider_item(
            INBOX_DIVIDER_ITEM_ID,
            INBOX_DIVIDER_ZONE_ID,
            center,
            width,
            content_size,
        ));
    }
    let rest = if has_rest {
        Some(LayoutRest {
            z: 0,
            transform: Transform::new(-1.0, -1.0, rest_size, content_size),
        })
    } else {
        None
    };
    Some(ShellPublication {
        lease: Lease {
            id: message.lease.id,
            generation: message.lease.generation,
        },
        revision: message.revision,
        delegated,
        needs_rest_delegation: needs_rest,
        fragment: LayoutFragment {
            layout: ResolvedLayout {
                items: layout_items,
                zones,
                dividers,
            },
            rest,
        },
        focus: requested_focus,
    })
}

fn serialize_host_message<T: Serialize>(message: &T) -> Option<ActorCommand> {
    serde_json::to_vec(message)
        .ok()
        .map(ActorCommand::SendHostMessage)
}

fn publish_layout(
    state: &SwitcherState,
    allow_delegation: bool,
    requested_focus: Option<u64>,
) -> Vec<ActorCommand> {
    let Some(publication) = resolve_shell_layout(state, allow_delegation, requested_focus) else {
        return Vec::new();
    };
    let mut commands = Vec::with_capacity(3);
    if let Some(command) = serialize_host_message(&SetPointerPolicy {
        message_type: "set-pointer-policy",
        lease: publication.lease,
        provider: "workspace",
    }) {
        commands.push(command);
    }
    if publication.needs_rest_delegation
        && let Some(command) = serialize_host_message(&DelegateLayoutRest {
            message_type: "delegate-layout-rest",
            lease: publication.lease,
            revision: publication.revision,
            actor: "workspace",
            panes: &publication.delegated,
        })
    {
        commands.push(command);
    }
    if let Some(command) = serialize_host_message(&SetResolvedLayout {
        message_type: "set-resolved-layout",
        lease: publication.lease,
        revision: publication.revision,
        layout: &publication.fragment.layout,
        rest: publication.fragment.rest.as_ref(),
        focus: publication.focus,
    }) {
        commands.push(command);
    }
    commands
}

fn publish_synchronized_layout(
    state: &mut SwitcherState,
    requested_focus: Option<u64>,
    reason: &'static str,
    native_resize_revision: Option<u64>,
) -> Vec<ActorCommand> {
    if matches!(reason, "picker" | "keybindings") && !state.pending_shell_layouts.is_empty() {
        state.deferred_overlay_layout = Some(DeferredOverlayLayout {
            state: shell_layout_state(state, requested_focus),
            reason,
        });
        return Vec::new();
    }
    publish_synchronized_layout_attempt(
        state,
        requested_focus,
        reason,
        native_resize_revision,
        0,
        None,
        None,
    )
}

fn publish_synchronized_pointer_layout(
    state: &mut SwitcherState,
    interaction: u64,
    event_sequence: u64,
    reason: &'static str,
) -> Vec<ActorCommand> {
    let focus = state.layout.as_ref().and_then(|layout| layout.focused_pane);
    publish_synchronized_layout_attempt(
        state,
        focus,
        reason,
        None,
        0,
        Some(interaction),
        Some(event_sequence),
    )
}

fn publish_synchronized_layout_attempt(
    state: &mut SwitcherState,
    requested_focus: Option<u64>,
    reason: &'static str,
    native_resize_revision: Option<u64>,
    native_retry_count: u8,
    interaction: Option<u64>,
    pointer_event_sequence: Option<u64>,
) -> Vec<ActorCommand> {
    if state.pending_shell_layouts.len() >= MAX_PENDING_LAYOUT_REQUESTS {
        restore_latest_submitted_or_durable_shell_layout(state);
        state.error = true;
        state.status = "Layout transaction backlog is full".to_string();
        return Vec::new();
    }
    let Some(publication) = resolve_shell_layout(state, false, requested_focus) else {
        return Vec::new();
    };
    let pointer_selection = native_resize_revision.and_then(|_| {
        serde_json::to_vec(&SetPointerPolicy {
            message_type: "set-pointer-policy",
            lease: publication.lease,
            provider: "workspace",
        })
        .ok()
    });
    let LayoutFragment { layout, rest } = publication.fragment;
    let Some(context) = state.layout.as_ref() else {
        return Vec::new();
    };
    if rest.is_some() && !context.rest {
        // The initial root publication may have identified delegated panes
        // before the host has installed the child lease. Keep the requested
        // shell state, but do not submit a fragment whose rest slot cannot yet
        // validate. The next post-delegation layout-state republishes it.
        return Vec::new();
    }

    let explicit_panes = layout.pane_ids().collect::<BTreeSet<_>>();
    let shell_panes = [
        Some(state.pane),
        state.picker_pane,
        state.keybindings_pane,
        state.inbox_pane,
        state.status_bar_pane,
    ]
    .into_iter()
    .flatten()
    .collect::<BTreeSet<_>>();
    let mut target_panes = explicit_panes.clone();
    if rest.is_some() {
        target_panes.extend(
            context
                .presented_panes
                .iter()
                .copied()
                .filter(|pane| !shell_panes.contains(pane) || explicit_panes.contains(pane)),
        );
    }
    let wait_for_panes = context
        .waitable_panes
        .iter()
        .copied()
        .filter(|pane| target_panes.contains(pane))
        .collect::<Vec<_>>();

    let rest_width = rest
        .as_ref()
        .map(|rest| rest.transform.size.u * f64::from(context.size.w.max(1)) / 2.0)
        .unwrap_or(0.0);
    let rest_height = rest
        .as_ref()
        .map(|rest| rest.transform.size.v * f64::from(context.size.height.max(1)) / 2.0)
        .unwrap_or(0.0);
    let sidebar_width = layout.pane_extent(
        state.pane,
        Axis::Horizontal,
        f64::from(context.size.w.max(1)),
    );
    let inbox_width = state.inbox_pane.map_or(0.0, |pane| {
        layout.pane_extent(pane, Axis::Horizontal, f64::from(context.size.w.max(1)))
    });
    let status_height = state.status_bar_pane.map_or(0.0, |pane| {
        layout.pane_extent(pane, Axis::Vertical, f64::from(context.size.height.max(1)))
    });

    let Some(request_id) = state.next_layout_request.checked_add(1) else {
        state.error = true;
        state.status = "Layout transaction identity space is exhausted".to_string();
        return Vec::new();
    };
    state.next_layout_request = request_id;
    let event_sequence = pointer_event_sequence.unwrap_or(0);
    eprintln!(
        "session-switcher-layout-transaction request_id={request_id} reason={reason} event_sequence={event_sequence} native_resize_revision={} rest_width={rest_width:.3} rest_height={rest_height:.3} sidebar_width={sidebar_width:.3} inbox_width={inbox_width:.3} status_height={status_height:.3} focus={} wait_for_panes={wait_for_panes:?}",
        native_resize_revision.unwrap_or(0),
        requested_focus.unwrap_or(0),
    );
    let fragment = LayoutFragment { layout, rest };
    let Ok(body) = serde_json::to_vec(&SubmitSynchronizedLayout {
        message_type: "submit-synchronized-layout",
        request_id,
        event_sequence: pointer_event_sequence,
        interaction,
        native_resize_revision,
        lease: publication.lease,
        inventory_revision: context.revision,
        base_layout_revision: context.presented_layout_revision,
        fragment: &fragment,
        focus: requested_focus,
        wait_for_panes: &wait_for_panes,
        presentation: "scale-then-atomic",
    }) else {
        return Vec::new();
    };
    state.pending_shell_layouts.insert(
        request_id,
        PendingShellLayout {
            state: shell_layout_state(state, requested_focus),
            native_resize_revision,
            native_retry_count,
        },
    );
    let mut commands = Vec::with_capacity(2);
    if native_resize_revision.is_some() {
        let Some(pointer_selection) = pointer_selection else {
            return Vec::new();
        };
        commands.push(ActorCommand::SendHostMessage(pointer_selection));
    }
    commands.push(ActorCommand::SendHostMessage(body));
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
        if x >= state.width - 56.0 && project.git {
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
        SwitcherTarget::NewSession => request_creation_backend_picker(state),
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
        state.status = format!("Creating and reflinking {branch}…");
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
    if matches!(
        state.mode,
        EditMode::CreateLocalContainer | EditMode::CreateRemoteContainer
    ) {
        let query = state.query.trim();
        let (connection, image) = if state.mode == EditMode::CreateRemoteContainer {
            let Some((connection, image)) = query.split_once("::") else {
                state.error = true;
                state.status = "Use CONNECTION::IMAGE for a remote container".to_string();
                return Vec::new();
            };
            (
                Some(connection.trim().to_string()),
                image.trim().to_string(),
            )
        } else {
            (None, query.to_string())
        };
        if image.is_empty() || connection.as_ref().is_some_and(String::is_empty) {
            state.error = true;
            state.status = "Enter an OCI image and, when remote, a connection".to_string();
            return Vec::new();
        }
        state.error = false;
        state.status = format!("Creating container backend from {image}…");
        state.next_request();
        let body = serde_json::json!({
            "type": "deploy-container-backend",
            "image": image,
            "connection": connection,
        });
        let Ok(body) = serde_json::to_vec(&body) else {
            state.error = true;
            state.status = "Could not encode the container request".to_string();
            return Vec::new();
        };
        return vec![ActorCommand::SendHostMessage(body)];
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

fn begin_worktree_for_source(state: &mut SwitcherState, backend: u64, source_session: u64) -> bool {
    let project = state
        .project_groups()
        .into_iter()
        .find(|project| project.backend == backend && project.source_session == source_session);
    if let Some(project) = project {
        begin_worktree(state, &project);
        true
    } else {
        false
    }
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

fn paint_for_resize_or_record_error(state: &mut SwitcherState, paint: ResizePaintPlan) {
    let result = match paint {
        ResizePaintPlan::OrdinaryRedraw => redraw(state, None),
        ResizePaintPlan::TaggedRedraw(resize_epoch) => redraw(state, Some(resize_epoch))
            // A newer shell transaction can settle before this actor handles
            // its resize notification. The old epoch is then no longer a
            // valid staging target, but the pane still needs a current frame.
            .or_else(|_tagged_error| redraw(state, None)),
        ResizePaintPlan::Reuse(resize_epoch) => reuse_pane_content(state.pane, resize_epoch)
            .or_else(|_reuse_error| redraw(state, Some(resize_epoch)).map(|_| ())),
    };
    if let Err(error) = result {
        state.error = true;
        state.status = error;
    }
}

fn redraw_for_resize_or_record_error(state: &mut SwitcherState, resize_epoch: Option<u64>) {
    if state.pane == 0 {
        return;
    }
    if let Err(error) = redraw(state, resize_epoch) {
        state.error = true;
        state.status = error;
    }
}

fn redraw(state: &mut SwitcherState, resize_epoch: Option<u64>) -> Result<(), String> {
    let mut cache = state
        .text_cache
        .take()
        .unwrap_or_else(|| TextLayoutCache::new(state.pane));
    cache.set_pane(state.pane);
    cache.begin_frame();
    let result = redraw_with_cache(state, resize_epoch, &mut cache);
    if result.is_err() {
        cache.abort_frame();
    }
    state.text_cache = Some(cache);
    result
}

fn redraw_with_cache(
    state: &mut SwitcherState,
    resize_epoch: Option<u64>,
    cache: &mut TextLayoutCache,
) -> Result<(), String> {
    let mut packet = Packet::new();
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
        cache,
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
            EditMode::CreateLocalContainer => "image:tag…".to_string(),
            EditMode::CreateRemoteContainer => "connection::image:tag…".to_string(),
        }
    } else {
        format!("{}▏", state.query)
    };
    add_text(
        state,
        &mut packet,
        cache,
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
        cache,
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
                    cache,
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
                    cache,
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
                    cache,
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
                            rect: Rect::new(state.width - 52.0, y + 4.0, 40.0, 26.0),
                            radius: 4.0,
                            color: if worktree_pressed { PRESSED } else { HOVER },
                        });
                    }
                    add_text(
                        state,
                        &mut packet,
                        cache,
                        "+WT",
                        11.0,
                        YELLOW,
                        state.width - 48.0,
                        y + 8.0,
                        34.0,
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
                    cache,
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
                        cache,
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
                    cache,
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
        cache,
        &truncate_to_width(&footer, state.width - 28.0, 10.0),
        10.0,
        if state.error { RED } else { MUTED },
        SIDE_MARGIN,
        state.height - 21.0,
        state.width - 28.0,
    )?;

    cache.submit(resize_epoch, &packet)?;
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
    let button_width = if compact_button { 32.0 } else { 50.0 };
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
    _state: &SwitcherState,
    packet: &mut Packet,
    cache: &mut TextLayoutCache,
    text: &str,
    size: f32,
    color: Color,
    x: f32,
    y: f32,
    max_width: f32,
) -> Result<(), String> {
    cache
        .draw_text_clipped(packet, text, size, max_width, size + 4.0, color, x, y)
        .map_err(|error| format!("layout session switcher text: {error}"))?;
    Ok(())
}

bindings::export!(SessionSwitcher with_types_in bindings);

#[cfg(test)]
mod tests {
    use brood_plugin_sdk::exports::brood::plugin::actor::{PresentationScope, PresentationSession};
    use brood_protocol::backend_catalogue::{BackendCatalogueEntry, BackendSessionEntry};

    use super::{
        ActorCommand, EditMode, LayoutContext, LayoutLeaseToken, LayoutRegionResizeMessage,
        LayoutStateSize, ResizePaintPlan, SwitcherState, ViewSession, actor_realm,
        apply_backend_sessions, apply_layout_pointer_message,
        apply_layout_transaction_notification, apply_worktree_completion,
        authenticated_neovim_client, begin_worktree_for_source, creation_backend_items,
        ensure_default_terminal_ui, header_geometry, neovim_client_realm, neovim_ui_id,
        open_creation_backend_picker, publish_layout, publish_layout_region_response,
        publish_synchronized_layout, request_creation_backend_picker, request_editor,
        resize_paint_plan, sender_actor_is, shell_layout_state, shell_requested_focus,
        start_neovim_client, submit, toggle_diff, truncate_to_width,
        update_creation_backend_picker, valid_editor_actor_target,
    };

    fn host_command<'a>(commands: &'a [ActorCommand], message_type: &str) -> &'a [u8] {
        commands
            .iter()
            .find_map(|command| {
                let ActorCommand::SendHostMessage(body) = command else {
                    return None;
                };
                let value = serde_json::from_slice::<serde_json::Value>(body).ok()?;
                (value["type"] == message_type).then_some(body.as_slice())
            })
            .unwrap_or_else(|| panic!("missing host command type {message_type}"))
    }

    #[test]
    fn empty_startup_layout_shows_switcher_and_status_but_hides_inbox() {
        let mut state = SwitcherState::new();
        state.pane = 1;
        state.status_bar_pane = Some(2);
        state.inbox_pane = Some(3);
        state.picker_pane = Some(4);
        state.keybindings_pane = Some(5);
        let mut visible_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(visible_panes.extend_from_slice(&[1, 2, 3]).is_ok());
        state.layout = Some(LayoutContext {
            revision: 9,
            presented_layout_revision: 3,
            lease: LayoutLeaseToken {
                id: 4,
                generation: 2,
            },
            waitable_panes: serde_json_core::heapless::Vec::new(),
            presented_panes: serde_json_core::heapless::Vec::new(),
            visible_panes,
            focused_pane: Some(1),
            size: LayoutStateSize {
                w: 1280,
                height: 720,
            },
            rest: false,
        });

        let commands = publish_layout(&state, true, Some(1));
        assert_eq!(commands.len(), 2);
        let body = host_command(&commands, "set-resolved-layout");
        let policy = host_command(&commands, "set-pointer-policy");
        let policy: serde_json::Value =
            serde_json::from_slice(policy).expect("valid pointer-policy selection JSON");
        assert_eq!(policy["provider"], "workspace");
        let value: serde_json::Value = serde_json::from_slice(body).expect("valid layout JSON");
        let items = value["layout"]["items"].as_array().expect("layout items");
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|item| item["id"] == 1));
        let status = items
            .iter()
            .find(|item| item["id"] == 2)
            .expect("status bar item");
        assert_eq!(status["input"], "pane");
        let switcher = items
            .iter()
            .find(|item| item["id"] == 1)
            .expect("switcher item");
        assert_eq!(switcher["transform"]["offset"]["u"], 0.5);
        assert_eq!(switcher["transform"]["size"]["u"], 0.5);
        assert!(!items.iter().any(|item| item["id"] == 3));
        assert!(value.get("rest").is_none());
    }

    #[test]
    fn shell_toggle_layout_keeps_edge_anchored_panes_inside_normalized_bounds() {
        let mut state = SwitcherState::new();
        state.pane = u64::MAX;
        state.keybindings_pane = Some(u64::MAX - 2);
        state.inbox_pane = Some(u64::MAX - 3);
        state.status_bar_pane = Some(u64::MAX - 4);
        state.picker_pane = Some(u64::MAX - 1);
        state.inbox_visible = true;
        let workspace = 7;
        let mut visible_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(
            visible_panes
                .extend_from_slice(&[
                    state.pane,
                    state.picker_pane.unwrap(),
                    state.keybindings_pane.unwrap(),
                    state.inbox_pane.unwrap(),
                    state.status_bar_pane.unwrap(),
                    workspace,
                ])
                .is_ok()
        );
        state.layout = Some(LayoutContext {
            revision: 23,
            presented_layout_revision: 22,
            lease: LayoutLeaseToken {
                id: 4,
                generation: 2,
            },
            waitable_panes: visible_panes.clone(),
            presented_panes: visible_panes.clone(),
            visible_panes,
            focused_pane: Some(workspace),
            size: LayoutStateSize {
                w: 1365,
                height: 767,
            },
            rest: true,
        });

        for sidebar_visible in [false, true] {
            state.sidebar_visible = sidebar_visible;
            let commands =
                publish_synchronized_layout(&mut state, Some(workspace), "sidebar", None);
            let value: serde_json::Value =
                serde_json::from_slice(host_command(&commands, "submit-synchronized-layout"))
                    .expect("valid toggle layout JSON");
            for item in value["fragment"]["layout"]["items"]
                .as_array()
                .expect("layout items")
            {
                for axis in ["u", "v"] {
                    let offset = item["transform"]["offset"][axis]
                        .as_f64()
                        .expect("numeric offset");
                    let size = item["transform"]["size"][axis]
                        .as_f64()
                        .expect("numeric size");
                    assert!(offset >= -1.0, "item {} starts before {axis}", item["id"]);
                    assert!(
                        offset + size <= 1.0,
                        "item {} ends after {axis}: {offset} + {size}",
                        item["id"]
                    );
                }
            }
            let status = value["fragment"]["layout"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == u64::MAX - 4)
                .expect("status bar item");
            assert_eq!(
                status["transform"]["offset"]["v"].as_f64().unwrap()
                    + status["transform"]["size"]["v"].as_f64().unwrap(),
                1.0
            );
        }
    }

    #[test]
    fn region_resize_returns_one_exact_epoch_fragment_and_restores_durable_size() {
        let mut state = SwitcherState::new();
        state.pane = 1;
        state.status_bar_pane = Some(2);
        state.inbox_pane = Some(3);
        state.picker_pane = Some(4);
        state.keybindings_pane = Some(5);
        let mut visible_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(visible_panes.extend_from_slice(&[1, 2, 3, 4, 5, 6]).is_ok());
        state.layout = Some(LayoutContext {
            revision: 9,
            presented_layout_revision: 3,
            lease: LayoutLeaseToken {
                id: 4,
                generation: 2,
            },
            waitable_panes: visible_panes.clone(),
            presented_panes: visible_panes.clone(),
            visible_panes,
            focused_pane: Some(6),
            size: LayoutStateSize {
                w: 800,
                height: 600,
            },
            rest: true,
        });
        let bytes = br#"{"type":"layout-region-resized","resize_epoch":{"id":31,"lease":1,"lease_generation":1,"base_layout_revision":3,"target_layout_revision":4},"lease":{"id":4,"generation":2},"size":{"w":1200,"h":700},"scale":1.5}"#;
        let (message, consumed) =
            serde_json_core::from_slice::<LayoutRegionResizeMessage>(bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(message.message_type.as_str(), "layout-region-resized");
        assert_eq!(message.resize_epoch.id, 31);
        assert_eq!((message.size.w, message.size.height), (1200, 700));
        assert!((message.scale - 1.5).abs() < f32::EPSILON);
        let commands = publish_layout_region_response(&mut state, &message);
        let body = host_command(&commands, "submit-layout-region");
        let response: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(response["resize_epoch"]["id"], 31);
        assert_eq!(
            response["lease"],
            serde_json::json!({"id": 4, "generation": 2})
        );
        assert_eq!(response["inventory_revision"], 9);
        assert!(response["fragment"]["rest"].is_object());
        let restored = state.layout.as_ref().unwrap().size;
        assert_eq!((restored.w, restored.height), (800, 600));
    }

    #[test]
    fn picker_and_keybinding_surfaces_are_explicit_overlay_panes() {
        let mut state = SwitcherState::new();
        state.pane = 1;
        state.status_bar_pane = Some(2);
        state.inbox_pane = Some(3);
        state.picker_pane = Some(4);
        state.keybindings_pane = Some(5);
        state.picker_visible = true;
        state.keybindings_visible = true;
        let mut visible_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(visible_panes.extend_from_slice(&[1, 2, 3, 4, 5]).is_ok());
        state.layout = Some(LayoutContext {
            revision: 9,
            presented_layout_revision: 3,
            lease: LayoutLeaseToken {
                id: 4,
                generation: 2,
            },
            waitable_panes: serde_json_core::heapless::Vec::new(),
            presented_panes: serde_json_core::heapless::Vec::new(),
            visible_panes,
            focused_pane: Some(4),
            size: LayoutStateSize {
                w: 1280,
                height: 720,
            },
            rest: false,
        });

        let commands = publish_layout(&state, true, Some(5));
        let value: serde_json::Value =
            serde_json::from_slice(host_command(&commands, "set-resolved-layout"))
                .expect("valid layout JSON");
        let items = value["layout"]["items"].as_array().expect("layout items");
        for pane in [4, 5] {
            let item = items
                .iter()
                .find(|item| item["id"] == pane)
                .expect("overlay pane item");
            assert_eq!(item["kind"]["type"], "overlay-pane");
            assert_eq!(item["kind"]["pane"], pane);
        }
    }

    #[test]
    fn initial_root_layout_waits_for_the_complete_shell_catalogue() {
        let mut state = SwitcherState::new();
        state.pane = 1;
        let mut visible_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(visible_panes.extend_from_slice(&[1, 2, 3, 4, 5, 6]).is_ok());
        state.layout = Some(LayoutContext {
            revision: 9,
            presented_layout_revision: 3,
            lease: LayoutLeaseToken {
                id: 4,
                generation: 2,
            },
            waitable_panes: serde_json_core::heapless::Vec::new(),
            presented_panes: serde_json_core::heapless::Vec::new(),
            visible_panes,
            focused_pane: Some(6),
            size: LayoutStateSize {
                w: 1280,
                height: 720,
            },
            rest: false,
        });

        state.picker_pane = Some(2);
        state.keybindings_pane = Some(3);
        state.inbox_pane = Some(4);
        assert!(publish_layout(&state, true, Some(6)).is_empty());

        state.status_bar_pane = Some(5);
        let commands = publish_layout(&state, true, Some(6));
        assert_eq!(commands.len(), 3);
        let delegate = host_command(&commands, "delegate-layout-rest");
        let layout = host_command(&commands, "set-resolved-layout");
        let delegate: serde_json::Value =
            serde_json::from_slice(delegate).expect("valid delegation JSON");
        assert_eq!(delegate["panes"], serde_json::json!([6]));
        let layout: serde_json::Value = serde_json::from_slice(layout).expect("valid layout JSON");
        assert!(
            layout["layout"]["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["id"] == 5))
        );
    }

    #[test]
    fn sidebar_transaction_carries_focus_native_revision_and_visible_subtree_wait_set() {
        let mut state = SwitcherState::new();
        state.pane = 1;
        state.picker_pane = Some(2);
        state.keybindings_pane = Some(3);
        state.inbox_pane = Some(4);
        state.status_bar_pane = Some(5);
        state.inbox_visible = true;
        let mut visible_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(visible_panes.extend_from_slice(&[1, 2, 3, 4, 5, 6]).is_ok());
        let mut waitable_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(
            waitable_panes
                .extend_from_slice(&[1, 2, 3, 4, 5, 6])
                .is_ok()
        );
        let mut presented_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(presented_panes.extend_from_slice(&[1, 2, 5, 6]).is_ok());
        state.layout = Some(LayoutContext {
            revision: 9,
            presented_layout_revision: 17,
            lease: LayoutLeaseToken {
                id: 4,
                generation: 2,
            },
            waitable_panes,
            presented_panes,
            visible_panes,
            focused_pane: Some(6),
            size: LayoutStateSize {
                w: 1280,
                height: 720,
            },
            rest: true,
        });

        let commands = publish_synchronized_layout(&mut state, Some(4), "native", Some(23));
        let [
            ActorCommand::SendHostMessage(policy),
            ActorCommand::SendHostMessage(body),
        ] = commands.as_slice()
        else {
            panic!("native update did not select policy before its synchronized transaction");
        };
        let policy: serde_json::Value =
            serde_json::from_slice(policy).expect("pointer-policy JSON");
        assert_eq!(policy["type"], "set-pointer-policy");
        assert_eq!(
            policy["lease"],
            serde_json::json!({"id": 4, "generation": 2})
        );
        assert_eq!(policy["provider"], "workspace");
        let value: serde_json::Value = serde_json::from_slice(body).expect("transaction JSON");
        assert_eq!(value["type"], "submit-synchronized-layout");
        assert_eq!(value["request_id"], 1);
        assert!(value.get("event_sequence").is_none());
        assert_eq!(value["native_resize_revision"], 23);
        assert_eq!(value["inventory_revision"], 9);
        assert_eq!(value["base_layout_revision"], 17);
        assert_eq!(value["focus"], 4);
        assert_eq!(value["wait_for_panes"], serde_json::json!([1, 4, 5, 6]));
        assert!(value["fragment"]["rest"].is_object());
        assert_eq!(state.pending_shell_layouts.len(), 1);

        assert!(apply_layout_transaction_notification(
            &mut state,
            &super::decode_layout_transaction_notification(
                br#"{"type":"completed","request_id":1,"resize_epoch":{"id":31,"lease":4,"lease_generation":2,"base_layout_revision":17,"target_layout_revision":18}}"#,
            )
            .expect("completed notification"),
        )
        .is_empty());
        state.sidebar_visible = false;
        let follow_up = publish_synchronized_layout(&mut state, Some(6), "sidebar", None);
        let [ActorCommand::SendHostMessage(body)] = follow_up.as_slice() else {
            panic!("follow-up sidebar update did not produce a synchronized transaction");
        };
        let value: serde_json::Value =
            serde_json::from_slice(body).expect("follow-up transaction JSON");
        assert_eq!(value["request_id"], 2);
        assert_eq!(value["base_layout_revision"], 18);
    }

    #[test]
    fn three_column_shell_dividers_resize_main_sidebar_and_inbox_atomically() {
        let mut state = SwitcherState::new();
        state.pane = 1;
        state.picker_pane = Some(2);
        state.keybindings_pane = Some(3);
        state.inbox_pane = Some(4);
        state.status_bar_pane = Some(5);
        state.inbox_visible = true;
        let mut visible_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(visible_panes.extend_from_slice(&[1, 2, 3, 4, 5, 6]).is_ok());
        let mut waitable_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(waitable_panes.extend_from_slice(&[1, 4, 5, 6]).is_ok());
        let mut presented_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(presented_panes.extend_from_slice(&[1, 4, 5, 6]).is_ok());
        state.layout = Some(LayoutContext {
            revision: 9,
            presented_layout_revision: 17,
            lease: LayoutLeaseToken {
                id: 4,
                generation: 2,
            },
            waitable_panes,
            presented_panes,
            visible_panes,
            focused_pane: Some(6),
            size: LayoutStateSize {
                w: 1280,
                height: 720,
            },
            rest: true,
        });
        state.durable_shell_layout = Some(shell_layout_state(&state, Some(6)));

        let initial = publish_layout(&state, false, Some(6));
        assert_eq!(initial.len(), 2);
        let initial = host_command(&initial, "set-resolved-layout");
        let initial: serde_json::Value =
            serde_json::from_slice(initial).expect("initial layout JSON");
        assert_eq!(initial["layout"]["zones"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            initial["layout"]["dividers"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(
            initial["layout"]["zones"][0]["id"],
            super::SIDEBAR_DIVIDER_ZONE_ID
        );
        assert_eq!(
            initial["layout"]["zones"][1]["id"],
            super::INBOX_DIVIDER_ZONE_ID
        );

        let pointer = |zone: u64, phase: &str, sequence: u64, u: f64, button: Option<u32>| {
            serde_json::from_value::<super::LayoutPointerMessage>(serde_json::json!({
                "type": "layout-pointer",
                "interaction": 99,
                "event_sequence": sequence,
                "lease": {"id": 4, "generation": 2},
                "zone": zone,
                "phase": phase,
                "position": {"u": u, "v": 0.0},
                "button": button,
                "inventory_revision": 9,
                "base_layout_revision": 17,
            }))
            .expect("pointer message")
        };
        assert!(
            apply_layout_pointer_message(
                &mut state,
                &pointer(
                    super::SIDEBAR_DIVIDER_ZONE_ID,
                    "button-press",
                    1,
                    0.0,
                    Some(super::PRIMARY_BUTTON),
                ),
            )
            .is_empty()
        );
        let motion = apply_layout_pointer_message(
            &mut state,
            &pointer(super::SIDEBAR_DIVIDER_ZONE_ID, "motion", 2, -0.125, None),
        );
        let [ActorCommand::SendHostMessage(motion)] = motion.as_slice() else {
            panic!("sidebar divider motion did not submit one synchronized layout");
        };
        let motion: serde_json::Value = serde_json::from_slice(motion).expect("motion JSON");
        assert_eq!(motion["interaction"], 99);
        assert_eq!(motion["event_sequence"], 2);
        assert_eq!(motion["wait_for_panes"], serde_json::json!([1, 4, 5, 6]));
        let rest_width = motion["fragment"]["rest"]["transform"]["size"]["u"]
            .as_f64()
            .expect("rest width")
            * 640.0;
        let sidebar = motion["fragment"]["layout"]["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|item| item["id"] == 1)
            .expect("sidebar");
        let sidebar_width = sidebar["transform"]["size"]["u"]
            .as_f64()
            .expect("sidebar width")
            * 640.0;
        assert!((rest_width - 560.0).abs() < 0.01);
        assert!((sidebar_width - 400.0).abs() < 0.01);
        assert!((state.inbox_width - 320.0).abs() < f64::EPSILON);

        assert!(apply_layout_transaction_notification(
            &mut state,
            &super::decode_layout_transaction_notification(
                br#"{"type":"completed","request_id":1,"resize_epoch":{"id":31,"lease":4,"lease_generation":2,"base_layout_revision":17,"target_layout_revision":18}}"#,
            )
            .expect("completed divider transaction"),
        )
        .is_empty());
        assert_eq!(
            state
                .durable_shell_layout
                .as_ref()
                .map(|layout| layout.sidebar_width),
            Some(400.0)
        );
        assert!(
            apply_layout_pointer_message(
                &mut state,
                &pointer(super::SIDEBAR_DIVIDER_ZONE_ID, "cancel", 3, -0.125, None,),
            )
            .is_empty()
        );
        assert!(
            apply_layout_pointer_message(
                &mut state,
                &pointer(
                    super::INBOX_DIVIDER_ZONE_ID,
                    "button-press",
                    4,
                    0.5,
                    Some(super::PRIMARY_BUTTON),
                ),
            )
            .is_empty()
        );
        let inbox_motion = apply_layout_pointer_message(
            &mut state,
            &pointer(super::INBOX_DIVIDER_ZONE_ID, "motion", 5, 0.625, None),
        );
        let [ActorCommand::SendHostMessage(inbox_motion)] = inbox_motion.as_slice() else {
            panic!("inbox divider motion did not submit one synchronized layout");
        };
        let inbox_motion: serde_json::Value =
            serde_json::from_slice(inbox_motion).expect("inbox motion JSON");
        assert_eq!(inbox_motion["request_id"], 2);
        assert_eq!(inbox_motion["event_sequence"], 5);
        let sidebar = inbox_motion["fragment"]["layout"]["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|item| item["id"] == 1)
            .expect("sidebar");
        let inbox = inbox_motion["fragment"]["layout"]["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|item| item["id"] == 4)
            .expect("inbox");
        assert!(
            (sidebar["transform"]["size"]["u"]
                .as_f64()
                .expect("sidebar width")
                * 640.0
                - 480.0)
                .abs()
                < 0.01
        );
        assert!(
            (inbox["transform"]["size"]["u"]
                .as_f64()
                .expect("inbox width")
                * 640.0
                - 240.0)
                .abs()
                < 0.01
        );
    }

    #[test]
    fn busy_native_transaction_retries_the_same_nonce_with_a_strict_bound() {
        let mut state = SwitcherState::new();
        state.pane = 1;
        state.picker_pane = Some(2);
        state.keybindings_pane = Some(3);
        state.inbox_pane = Some(4);
        state.status_bar_pane = Some(5);
        let mut visible_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(visible_panes.extend_from_slice(&[1, 2, 3, 4, 5, 6]).is_ok());
        let mut waitable_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(
            waitable_panes
                .extend_from_slice(&[1, 2, 3, 4, 5, 6])
                .is_ok()
        );
        let mut presented_panes = serde_json_core::heapless::Vec::<u64, 64>::new();
        assert!(presented_panes.extend_from_slice(&[1, 5, 6]).is_ok());
        state.layout = Some(LayoutContext {
            revision: 9,
            presented_layout_revision: 17,
            lease: LayoutLeaseToken {
                id: 4,
                generation: 2,
            },
            waitable_panes,
            presented_panes,
            visible_panes,
            focused_pane: Some(6),
            size: LayoutStateSize {
                w: 1280,
                height: 720,
            },
            rest: true,
        });
        state.native_resize_revision = Some(23);
        let first = publish_synchronized_layout(&mut state, Some(6), "native", Some(23));
        assert_eq!(first.len(), 2);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(host_command(&first, "set-pointer-policy"))
                .expect("initial native pointer policy")["lease"],
            serde_json::json!({"id": 4, "generation": 2})
        );

        for rejected_request in 1..=2 {
            let notification = format!(
                r#"{{"type":"rejected","request_id":{rejected_request},"reason":"busy","message":"transient host backpressure"}}"#
            );
            let commands = apply_layout_transaction_notification(
                &mut state,
                &super::decode_layout_transaction_notification(notification.as_bytes())
                    .expect("busy rejection"),
            );
            let [
                ActorCommand::SendHostMessage(policy),
                ActorCommand::SendHostMessage(body),
            ] = commands.as_slice()
            else {
                panic!("busy native rejection did not select policy before its retry");
            };
            let policy: serde_json::Value =
                serde_json::from_slice(policy).expect("native retry pointer-policy JSON");
            assert_eq!(policy["type"], "set-pointer-policy");
            assert_eq!(
                policy["lease"],
                serde_json::json!({"id": 4, "generation": 2})
            );
            let request: serde_json::Value =
                serde_json::from_slice(body).expect("native retry JSON");
            assert_eq!(request["request_id"], rejected_request + 1);
            assert_eq!(request["native_resize_revision"], 23);
            assert_eq!(request["base_layout_revision"], 17);
        }

        let exhausted = apply_layout_transaction_notification(
            &mut state,
            &super::decode_layout_transaction_notification(
                br#"{"type":"rejected","request_id":3,"reason":"busy","message":"still busy"}"#,
            )
            .expect("final busy rejection"),
        );
        assert!(exhausted.is_empty());
        assert!(state.pending_shell_layouts.is_empty());
        assert_eq!(state.native_resize_revision, Some(23));
    }

    #[test]
    fn layout_backlog_overflow_restores_the_latest_submitted_shell_proposal() {
        let mut state = SwitcherState::new();
        state.layout = Some(LayoutContext {
            revision: 5,
            presented_layout_revision: 7,
            lease: LayoutLeaseToken {
                id: 1,
                generation: 1,
            },
            waitable_panes: serde_json_core::heapless::Vec::new(),
            presented_panes: serde_json_core::heapless::Vec::new(),
            visible_panes: serde_json_core::heapless::Vec::new(),
            focused_pane: Some(9),
            size: LayoutStateSize {
                w: 1280,
                height: 720,
            },
            rest: false,
        });
        state.sidebar_visible = false;
        state.picker_visible = true;
        state.keybindings_visible = false;
        state.inbox_visible = true;
        let latest_submitted = shell_layout_state(&state, Some(9));
        for request_id in 1..=super::MAX_PENDING_LAYOUT_REQUESTS as u64 {
            state.pending_shell_layouts.insert(
                request_id,
                super::PendingShellLayout {
                    state: latest_submitted.clone(),
                    native_resize_revision: None,
                    native_retry_count: 0,
                },
            );
        }

        // The caller applies its requested state before it asks the common
        // publisher to serialize the transaction. This 65th target must not
        // survive when the bounded pending set cannot record it.
        state.sidebar_visible = true;
        state.picker_visible = false;
        state.keybindings_visible = true;
        state.inbox_visible = false;
        if let Some(layout) = state.layout.as_mut() {
            layout.focused_pane = Some(1);
        }
        let commands = publish_synchronized_layout(&mut state, Some(1), "sidebar", None);

        assert!(commands.is_empty());
        assert_eq!(
            state.pending_shell_layouts.len(),
            super::MAX_PENDING_LAYOUT_REQUESTS
        );
        assert_eq!(
            shell_layout_state(
                &state,
                state.layout.as_ref().and_then(|layout| layout.focused_pane)
            ),
            latest_submitted
        );
        assert!(state.error);
        assert_eq!(state.status, "Layout transaction backlog is full");
    }

    #[test]
    fn rejected_sidebar_transaction_rolls_back_but_completed_state_becomes_durable() {
        let mut state = SwitcherState::new();
        state.durable_shell_layout = Some(shell_layout_state(&state, Some(9)));
        state.layout = Some(LayoutContext {
            revision: 5,
            presented_layout_revision: 7,
            lease: LayoutLeaseToken {
                id: 1,
                generation: 1,
            },
            waitable_panes: serde_json_core::heapless::Vec::new(),
            presented_panes: serde_json_core::heapless::Vec::new(),
            visible_panes: serde_json_core::heapless::Vec::new(),
            focused_pane: Some(9),
            size: LayoutStateSize {
                w: 1280,
                height: 720,
            },
            rest: false,
        });
        state.sidebar_visible = false;
        state.pending_shell_layouts.insert(
            1,
            super::PendingShellLayout {
                state: shell_layout_state(&state, Some(9)),
                native_resize_revision: None,
                native_retry_count: 0,
            },
        );
        assert!(
            apply_layout_transaction_notification(
                &mut state,
                &super::decode_layout_transaction_notification(
                    br#"{"type":"rejected","request_id":1}"#,
                )
                .expect("rejected notification"),
            )
            .is_empty()
        );
        assert!(state.sidebar_visible);
        assert!(state.pending_shell_layouts.is_empty());

        state.sidebar_visible = false;
        state.pending_shell_layouts.insert(
            3,
            super::PendingShellLayout {
                state: shell_layout_state(&state, Some(9)),
                native_resize_revision: None,
                native_retry_count: 0,
            },
        );
        assert!(
            apply_layout_transaction_notification(
                &mut state,
                &super::decode_layout_transaction_notification(
                    br#"{"type":"aborted","request_id":3,"resize_epoch":{"id":43,"lease":1,"lease_generation":1,"base_layout_revision":7,"target_layout_revision":9},"reason":"stale-inventory-revision","message":"pane removed"}"#,
                )
                .expect("aborted notification"),
            )
            .is_empty()
        );
        assert!(state.sidebar_visible);
        assert!(state.pending_shell_layouts.is_empty());

        state.sidebar_visible = false;
        state.pending_shell_layouts.insert(
            2,
            super::PendingShellLayout {
                state: shell_layout_state(&state, Some(9)),
                native_resize_revision: None,
                native_retry_count: 0,
            },
        );
        assert!(apply_layout_transaction_notification(
            &mut state,
            &super::decode_layout_transaction_notification(
                br#"{"type":"completed","request_id":2,"resize_epoch":{"id":41,"lease":1,"lease_generation":1,"base_layout_revision":7,"target_layout_revision":8}}"#,
            )
            .expect("completed notification"),
        )
        .is_empty());
        assert!(!state.sidebar_visible);
        assert_eq!(
            state
                .durable_shell_layout
                .as_ref()
                .map(|layout| layout.sidebar_visible),
            Some(false)
        );
        assert_eq!(
            state
                .layout
                .as_ref()
                .map(|layout| layout.presented_layout_revision),
            Some(8)
        );
    }

    #[test]
    fn stale_inventory_rejection_preserves_hidden_picker_and_requested_focus() {
        let mut state = SwitcherState::new();
        state.picker_pane = Some(2);
        state.picker_visible = true;
        state.durable_shell_layout = Some(shell_layout_state(&state, Some(2)));
        state.layout = Some(LayoutContext {
            revision: 6,
            presented_layout_revision: 8,
            lease: LayoutLeaseToken {
                id: 1,
                generation: 1,
            },
            waitable_panes: serde_json_core::heapless::Vec::new(),
            presented_panes: serde_json_core::heapless::Vec::new(),
            visible_panes: serde_json_core::heapless::Vec::new(),
            focused_pane: Some(9),
            size: LayoutStateSize {
                w: 1280,
                height: 720,
            },
            rest: true,
        });
        state.picker_visible = false;
        state.pending_shell_layouts.insert(
            4,
            super::PendingShellLayout {
                state: shell_layout_state(&state, Some(9)),
                native_resize_revision: None,
                native_retry_count: 0,
            },
        );

        assert!(
            apply_layout_transaction_notification(
                &mut state,
                &super::decode_layout_transaction_notification(
                    br#"{"type":"rejected","request_id":4,"reason":"stale-inventory-revision"}"#,
                )
                .expect("stale inventory rejection"),
            )
            .is_empty()
        );
        assert!(!state.picker_visible);
        assert_eq!(
            state.layout.as_ref().and_then(|layout| layout.focused_pane),
            Some(9)
        );
        assert!(state.pending_shell_layouts.is_empty());
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
            resize_paint_plan((220.0, 240.0, 1.0), (230.0, 240.0, 1.0), Some(43)),
            ResizePaintPlan::TaggedRedraw(43),
            "clamped UI dimensions must not hide a real allocation change"
        );
        assert_eq!(
            resize_paint_plan((320.0, 720.0, 1.0), (320.0, 720.0, 2.0), Some(44)),
            ResizePaintPlan::TaggedRedraw(44),
            "content from a different output scale is not reusable"
        );
    }

    #[test]
    fn visible_shell_overlay_keeps_focus_across_layout_state_revisions() {
        let mut state = SwitcherState::new();
        state.inbox_pane = Some(3);
        state.inbox_visible = true;
        assert_eq!(
            shell_requested_focus(&state, Some(9)),
            Some(9),
            "the non-modal inbox must allow its target content to retain focus"
        );

        state.picker_pane = Some(4);
        state.picker_visible = true;
        assert_eq!(shell_requested_focus(&state, Some(9)), Some(4));

        state.keybindings_pane = Some(5);
        state.keybindings_visible = true;
        assert_eq!(shell_requested_focus(&state, Some(9)), Some(5));

        state.keybindings_visible = false;
        state.picker_visible = false;
        state.inbox_visible = false;
        assert_eq!(shell_requested_focus(&state, Some(9)), Some(9));
    }

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
        assert!(matches!(
            first.presentation_scope,
            PresentationScope::Session(PresentationSession {
                backend_id: 1,
                session_id: 7,
            })
        ));

        let Some(ActorCommand::StartActor(second)) = ensure_default_terminal_ui(&mut state, 2, 7)
        else {
            panic!("second backend did not start a terminal frontend");
        };
        assert_eq!(second.id, "terminal-ui-b2-s7");
        assert!(matches!(
            second.presentation_scope,
            PresentationScope::Session(PresentationSession {
                backend_id: 2,
                session_id: 7,
            })
        ));
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
        assert!(matches!(
            started.presentation_scope,
            PresentationScope::Session(PresentationSession {
                backend_id: 4,
                session_id: 27,
            })
        ));
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
    fn diff_viewers_are_independent_toggleable_session_items() {
        let mut state = SwitcherState::new();
        let first = toggle_diff(&mut state, 3, 9);
        let [ActorCommand::StartActor(started)] = first.as_slice() else {
            panic!("first diff request did not start a viewer");
        };
        assert_eq!(started.id, "diff-viewer-sb3-s9");
        assert!(matches!(
            started.presentation_scope,
            PresentationScope::Session(PresentationSession {
                backend_id: 3,
                session_id: 9,
            })
        ));
        assert_eq!(
            started.capabilities,
            ["filesystem:workspace-read", "subprocess:git"]
        );

        let other = toggle_diff(&mut state, 3, 10);
        let [ActorCommand::StartActor(other)] = other.as_slice() else {
            panic!("another session did not start its own viewer");
        };
        assert_eq!(other.id, "diff-viewer-sb3-s10");
        assert_eq!(state.diff_viewers.len(), 2);

        let second = toggle_diff(&mut state, 3, 9);
        let [ActorCommand::StopActor(stopped)] = second.as_slice() else {
            panic!("second diff request did not toggle off the viewer");
        };
        assert_eq!(stopped, "diff-viewer-sb3-s9");
        assert_eq!(state.diff_viewers.len(), 1);
        assert!(state.diff_viewers.contains("diff-viewer-sb3-s10"));
    }

    #[test]
    fn backend_picker_exposes_local_and_remote_container_creation() {
        let mut state = SwitcherState::new();
        apply_backend_sessions(
            &mut state,
            true,
            false,
            true,
            vec![BackendCatalogueEntry {
                backend: brood_protocol::BackendId(7),
                label: "Local".to_string(),
                sessions: Vec::new(),
            }],
        );
        let commands = open_creation_backend_picker(&mut state);
        let [ActorCommand::SendLocalMessage(message)] = commands.as_slice() else {
            panic!("backend picker did not open");
        };
        let value: serde_json::Value =
            serde_json::from_slice(&message.body).unwrap_or(serde_json::Value::Null);
        let actions = value["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["value"]["action"].as_str())
            .collect::<Vec<_>>();
        assert!(actions.contains(&"create-container-local"));
        assert!(actions.contains(&"create-container-remote"));
        assert!(actions.contains(&"choose-directory"));
        assert_eq!(value["items"][0]["label"], "Local");
        assert_eq!(value["items"][0]["value"]["backend"], 7);
        assert!(actions.contains(&"create-local"));
        assert!(value["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["label"] == "Local" && item["value"]["action"] == "create-local")
        }));
    }

    #[test]
    fn new_session_picker_offers_worktree_and_enters_branch_mode() {
        let mut state = SwitcherState::new();
        state.backends.push((7, "Local".to_string()));
        state.sessions.push(ViewSession {
            backend: 7,
            id: 3,
            name: "brood".to_string(),
            active: true,
            windows: 1,
            working_directory: Some("/work/brood".to_string()),
            project_id: "/work/brood/.git".to_string(),
            project_name: "brood".to_string(),
            branch: Some("main".to_string()),
            worktree: false,
            git: true,
        });

        let commands = open_creation_backend_picker(&mut state);
        let [ActorCommand::SendLocalMessage(message)] = commands.as_slice() else {
            panic!("creation picker did not open");
        };
        let value: serde_json::Value =
            serde_json::from_slice(&message.body).unwrap_or(serde_json::Value::Null);
        assert_eq!(value["title"], "Create session");
        assert_eq!(value["items"][0]["label"], "Worktree · brood");
        assert_eq!(value["items"][0]["detail"], "Create a new branch from main");
        assert_eq!(value["items"][0]["value"]["action"], "create-worktree");
        assert_eq!(value["items"][0]["value"]["backend"], 7);
        assert_eq!(value["items"][0]["value"]["session"], 3);

        assert!(begin_worktree_for_source(&mut state, 7, 3));
        assert!(state.mode == EditMode::CreateWorktree(7, 3));
        assert_eq!(state.status, "New branch in brood: Enter to create");
    }

    #[test]
    fn project_git_capability_is_independent_of_session_order() {
        let mut state = SwitcherState::new();
        for (id, git) in [(3, false), (4, true)] {
            state.sessions.push(ViewSession {
                backend: 7,
                id,
                name: format!("brood-{id}"),
                active: id == 3,
                windows: 1,
                working_directory: Some(format!("/work/brood-{id}")),
                project_id: "/work/brood/.git".to_string(),
                project_name: "brood".to_string(),
                branch: Some("main".to_string()),
                worktree: id == 4,
                git,
            });
        }

        let groups = state.project_groups();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].git);
        assert!(creation_backend_items(&state)
            .iter()
            .any(|item| item.value.action.as_deref() == Some("create-worktree")));
    }

    #[test]
    fn empty_initial_catalogue_preserves_the_live_backend_snapshot() {
        let mut state = SwitcherState::new();
        state.backend_id = 7;
        state.session_id = 3;
        state.sessions.push(ViewSession {
            backend: 7,
            id: 3,
            name: "brood".to_string(),
            active: true,
            windows: 1,
            working_directory: Some("/work/brood".to_string()),
            project_id: "/work/brood/.git".to_string(),
            project_name: "brood".to_string(),
            branch: Some("main".to_string()),
            worktree: false,
            git: true,
        });

        apply_backend_sessions(
            &mut state,
            true,
            false,
            false,
            vec![BackendCatalogueEntry {
                backend: brood_protocol::BackendId(7),
                label: "Local".to_string(),
                sessions: Vec::new(),
            }],
        );

        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].id, 3);
        assert!(creation_backend_items(&state)
            .iter()
            .any(|item| item.value.action.as_deref() == Some("create-worktree")));

        apply_backend_sessions(
            &mut state,
            true,
            false,
            false,
            vec![BackendCatalogueEntry {
                backend: brood_protocol::BackendId(7),
                label: "Local".to_string(),
                sessions: vec![BackendSessionEntry {
                    id: 8,
                    name: "replacement".to_string(),
                    active: true,
                    windows: 0,
                    working_directory: Some("/work/replacement".to_string()),
                    project_id: "/work/replacement/.git".to_string(),
                    project_name: "replacement".to_string(),
                    branch: Some("main".to_string()),
                    worktree: false,
                    git: true,
                    isolation: brood_protocol::SessionIsolationStatus::default(),
                }],
            }],
        );
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].id, 8);
    }

    #[test]
    fn worktree_completion_keeps_new_identity_until_catalogue_arrives() {
        let mut state = SwitcherState::new();
        state.backend_id = 1;
        state.session_id = 3;
        state.pending = Some(17);

        apply_worktree_completion(&mut state, 1, 17, true, Some(9), "");

        assert_eq!(state.backend_id, 1);
        assert_eq!(state.session_id, 9);
        assert_eq!(state.selected, Some((1, 9)));
        assert_eq!(state.pending, None);

        state.sessions.push(ViewSession {
            backend: 1,
            id: 9,
            name: "new-worktree".to_string(),
            active: true,
            windows: 0,
            working_directory: Some("/worktrees/new-worktree".to_string()),
            project_id: "/source/.git".to_string(),
            project_name: "source".to_string(),
            branch: Some("new-worktree".to_string()),
            worktree: true,
            git: true,
        });
        state.ensure_selection();

        assert_eq!(state.selected, Some((1, 9)));
        assert_eq!(state.selected_session().map(|session| session.id), Some(9));
    }

    #[test]
    fn worktree_failure_clears_progress_and_reports_a_specific_error() {
        let mut state = SwitcherState::new();
        state.pending = Some(17);
        state.mode = EditMode::CreateWorktree(1, 3);
        state.query = "new-branch".to_string();

        apply_worktree_completion(&mut state, 1, 17, false, None, "branch already exists");

        assert_eq!(state.pending, None);
        assert!(state.error);
        assert_eq!(
            state.status,
            "Worktree creation failed: branch already exists"
        );
        assert!(state.mode == EditMode::CreateWorktree(1, 3));
        assert_eq!(state.query, "new-branch");

        state.pending = Some(18);
        apply_worktree_completion(&mut state, 1, 18, false, None, "");
        assert_eq!(
            state.status,
            "Worktree creation failed: the backend returned no error details"
        );
    }

    #[test]
    fn new_session_opens_a_loading_picker_before_requesting_backends() {
        let mut state = SwitcherState::new();
        let commands = request_creation_backend_picker(&mut state);
        let [
            ActorCommand::SendLocalMessage(open),
            ActorCommand::SendHostMessage(discover),
        ] = commands.as_slice()
        else {
            panic!("new session did not open the picker before backend discovery");
        };
        let value: serde_json::Value =
            serde_json::from_slice(&open.body).unwrap_or(serde_json::Value::Null);
        assert_eq!(open.actor, "picker");
        assert_eq!(value["type"], "picker-create");
        assert_eq!(value["request"], "backend-discovery");
        assert_eq!(value["loading"], true);
        assert_eq!(value["loading_label"], "Discovering backends…");
        assert_eq!(discover, br#"{"type":"request-backend-catalogue"}"#);
    }

    #[test]
    fn completed_empty_discovery_offers_local_backend_creation() {
        let mut state = SwitcherState::new();
        state.backend_picker = Some(17);
        apply_backend_sessions(&mut state, true, false, false, Vec::new());
        let ActorCommand::SendLocalMessage(update) =
            update_creation_backend_picker(&state).expect("picker update")
        else {
            panic!("backend discovery did not update the picker");
        };
        let value: serde_json::Value =
            serde_json::from_slice(&update.body).unwrap_or(serde_json::Value::Null);
        assert_eq!(value["type"], "picker-update");
        assert_eq!(value["picker"], 17);
        assert_eq!(value["loading"], false);
        assert!(value["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["label"] == "Local" && item["value"]["action"] == "create-local")
        }));
    }

    #[test]
    fn backend_catalogue_wire_message_decodes_in_the_guest() {
        let message = brood_protocol::backend_catalogue::BackendCatalogueMessage::sessions(
            true,
            true,
            true,
            vec![BackendCatalogueEntry {
                backend: brood_protocol::BackendId(1),
                label: "Backend 1".to_string(),
                sessions: Vec::new(),
            }],
        );
        let bytes = serde_json::to_vec(&message).expect("catalogue JSON");
        let decoded = serde_json::from_slice::<
            brood_protocol::backend_catalogue::BackendCatalogueMessage,
        >(&bytes);
        assert_eq!(decoded.expect("decode catalogue JSON"), message);
    }

    #[test]
    fn remote_container_submission_preserves_connection_and_image() {
        let mut state = SwitcherState::new();
        state.mode = EditMode::CreateRemoteContainer;
        state.query = "builder::docker.io/library/alpine:latest".to_string();
        let commands = submit(&mut state);
        let [ActorCommand::SendHostMessage(body)] = commands.as_slice() else {
            panic!("remote container request was not sent to the host");
        };
        let value: serde_json::Value =
            serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
        assert_eq!(value["type"], "deploy-container-backend");
        assert_eq!(value["connection"], "builder");
        assert_eq!(value["image"], "docker.io/library/alpine:latest");
    }
}
