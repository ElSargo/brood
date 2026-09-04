use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
};

use brood_graphics::{Color, Command, Packet, Rect, theme::ashen};
use brood_plugin_sdk::{
    actor::{send_local, send_presentation, send_session, start_global_actor, start_session_actor},
    bindings,
    brood::plugin::host,
    exports::brood::plugin::actor::{
        ChildEvent, Command as ActorCommand, Event, Guest, LifecycleEvent, PresentationEvent,
        WorkerEvent, WorkerRequest,
    },
    graphics::{TextLayoutCache, drop_resources},
    input::BUTTON_PRIMARY as LEFT_BUTTON,
    runtime::write_all,
};
use serde::{Deserialize, Serialize};

const COMPONENT_FILE: &str = "brood_coding_agent_plugin.component.wasm";
const CLIENT_INIT_ID: &str = "brood.client-init";
const UI_ID: &str = "coding-agent-ui";
const INBOX_ID: &str = "inbox";
const FIRST_AGENT: &str = "coding-agent-1";
const WORKER_ID: u64 = 1;
const UPDATE_NOTIFICATION_TOKEN: u64 = 0x434f_4445_5841_4745;
const MAX_LOG_LINES: usize = 256;
const MAX_IMAGES: usize = 4;
const MAX_PROMPT_HISTORY: usize = 100;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 12 * 1024 * 1024;
const IMAGE_CHUNK_BYTES: usize = 20 * 1024;
const HEADER_HEIGHT: f32 = 76.0;
const PROMPT_HEIGHT: f32 = 92.0;
const TRANSCRIPT_GAP: f32 = 10.0;
const SCROLLBAR_WIDTH: f32 = 5.0;
const SCROLLBAR_HIT_WIDTH: f32 = 18.0;
const SEARCH_HEIGHT: f32 = 42.0;
const IMAGE_TRAY_HEIGHT: f32 = 112.0;
const IMAGE_TRAY_GAP: f32 = 8.0;
const MOD_CTRL: u32 = 1;
const MOD_SHIFT: u32 = 4;
const FOCUS_MESSAGE: &[u8] = br#"{"kind":"focus"}"#;

const BG: Color = ashen::BACKGROUND;
const PANEL: Color = ashen::SURFACE;
const PANEL_LIGHT: Color = ashen::SURFACE_RAISED;
const HOVER: Color = ashen::INTERACTIVE_HOVER;
const PRESSED: Color = ashen::INTERACTIVE_PRESSED;
const FG: Color = ashen::TEXT;
const MUTED: Color = ashen::MUTED;
const YELLOW: Color = ashen::WARNING;
const BLUE: Color = ashen::ACCENT;
const GREEN: Color = ashen::INFO;
const RED: Color = ashen::ERROR;
const STRONG: Color = ashen::TEXT_STRONG;
const BRIGHT: Color = ashen::TEXT_BRIGHT;
const SUNKEN: Color = ashen::SURFACE_SUNKEN;
const ACCENT_MUTED: Color = ashen::ACCENT_MUTED;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct WireMessage {
    kind: String,
    action: String,
    backend_id: u64,
    session_id: u64,
    agent: String,
    prompt: String,
    model: String,
    reasoning: String,
    status: String,
    log_kind: String,
    text: String,
    category: String,
    title: String,
    detail: String,
    target_actor: String,
    agents: Vec<WireAgent>,
    image_id: u64,
    image_ids: Vec<u64>,
    image_name: String,
    mime_type: String,
    total_bytes: u64,
    offset: u64,
    data: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct WireAgent {
    id: String,
    status: String,
    model: String,
    reasoning: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TurnRequest {
    agent: String,
    prompt: String,
    model: String,
    reasoning: String,
    session_id: u64,
    history: Vec<HistoryTurn>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HistoryTurn {
    user: String,
    assistant: String,
}

#[derive(Debug, Serialize)]
struct RunnerImage {
    path: String,
    mime_type: String,
}

#[derive(Debug, Serialize)]
struct RunnerRequest<'a> {
    agent: &'a str,
    prompt: &'a str,
    model: &'a str,
    reasoning: &'a str,
    session_id: u64,
    history: &'a [HistoryTurn],
    images: Vec<RunnerImage>,
}

#[derive(Clone, Debug)]
struct ImageAttachment {
    id: u64,
    name: String,
    mime_type: String,
    bytes: Vec<u8>,
}

struct ImagePreview {
    handle: u64,
    width: u32,
    height: u32,
}

struct IncomingImage {
    name: String,
    mime_type: String,
    total_bytes: usize,
    bytes: Vec<u8>,
}

#[derive(Clone)]
struct LogLine {
    kind: String,
    text: String,
}

struct RootState {
    client: bool,
    backend_id: u64,
    session_id: u64,
    agents: Vec<WireAgent>,
    subscribers: Vec<Subscriber>,
    next_agent: u64,
    next_ui: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Subscriber {
    Local(String),
    Presentation(String),
}

struct AgentState {
    id: String,
    session_id: u64,
    model: String,
    reasoning: String,
    status: String,
    subscribers: Vec<Subscriber>,
    active_worker: Option<u64>,
    active_target: Option<String>,
    notification: u64,
    published_generation: u64,
    observed_generation: u64,
    queued: VecDeque<(u64, WireMessage)>,
    incoming_images: BTreeMap<u64, IncomingImage>,
    draft_images: BTreeMap<u64, ImageAttachment>,
    active_images: Vec<ImageAttachment>,
    history: Vec<HistoryTurn>,
}

impl AgentState {
    fn new(session_id: u64) -> Self {
        Self {
            id: String::new(),
            session_id,
            model: "default".to_string(),
            reasoning: "medium".to_string(),
            status: "idle".to_string(),
            subscribers: Vec::new(),
            active_worker: None,
            active_target: None,
            notification: 0,
            published_generation: 0,
            observed_generation: 0,
            queued: VecDeque::new(),
            incoming_images: BTreeMap::new(),
            draft_images: BTreeMap::new(),
            active_images: Vec::new(),
            history: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct AgentView {
    status: String,
    model: String,
    reasoning: String,
    logs: VecDeque<LogLine>,
}

impl AgentView {
    fn from_wire(agent: &WireAgent) -> Self {
        Self {
            status: agent.status.clone(),
            model: agent.model.clone(),
            reasoning: agent.reasoning.clone(),
            logs: VecDeque::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum UiTarget {
    AgentTab(usize),
    NewAgent,
    Search,
    Model,
    Reasoning,
    Send,
    Prompt,
}

#[derive(Clone, Debug)]
struct TabDrag {
    agent: String,
    start_x: f32,
    moved: bool,
}

#[derive(Clone, Copy, Debug)]
struct ScrollbarDrag {
    pointer_offset: f32,
}

struct UiState {
    pane: u64,
    width: f32,
    height: f32,
    backend_id: u64,
    session_id: u64,
    agents: Vec<String>,
    views: BTreeMap<String, AgentView>,
    selected: usize,
    preferred_agent: Option<String>,
    prompt: String,
    prompt_cursor: usize,
    prompt_selection: Option<(usize, usize)>,
    prompt_history: VecDeque<String>,
    prompt_history_cursor: Option<usize>,
    prompt_history_draft: Option<String>,
    pane_focused: bool,
    transcript_scroll: f32,
    transcript_content_height: f32,
    transcript_viewport_height: f32,
    transcript_follow_tail: bool,
    scrollbar_drag: Option<ScrollbarDrag>,
    scrollbar_hovered: bool,
    search_active: bool,
    search_query: String,
    search_cursor: usize,
    search_match: usize,
    search_reveal_pending: bool,
    tab_drag: Option<TabDrag>,
    images: Vec<ImageAttachment>,
    image_previews: BTreeMap<u64, ImagePreview>,
    retired_image_previews: Vec<u64>,
    next_image: u64,
    suppressed_text: Option<String>,
    paste_key_pressed: bool,
    text_cache: Option<TextLayoutCache>,
    draw_error: Option<String>,
    updates_since_draw: usize,
    error: String,
    hovered: Option<UiTarget>,
    pressed: Option<UiTarget>,
    notification: u64,
    published_generation: u64,
    observed_generation: u64,
    outbox: VecDeque<(String, WireMessage)>,
}

impl UiState {
    fn new() -> Self {
        Self {
            pane: 0,
            width: 900.0,
            height: 700.0,
            backend_id: 0,
            session_id: 0,
            agents: Vec::new(),
            views: BTreeMap::new(),
            selected: 0,
            preferred_agent: None,
            prompt: String::new(),
            prompt_cursor: 0,
            prompt_selection: None,
            prompt_history: VecDeque::new(),
            prompt_history_cursor: None,
            prompt_history_draft: None,
            pane_focused: true,
            transcript_scroll: 0.0,
            transcript_content_height: 0.0,
            transcript_viewport_height: 0.0,
            transcript_follow_tail: true,
            scrollbar_drag: None,
            scrollbar_hovered: false,
            search_active: false,
            search_query: String::new(),
            search_cursor: 0,
            search_match: 0,
            search_reveal_pending: false,
            tab_drag: None,
            images: Vec::new(),
            image_previews: BTreeMap::new(),
            retired_image_previews: Vec::new(),
            next_image: 1,
            suppressed_text: None,
            paste_key_pressed: false,
            text_cache: None,
            draw_error: None,
            updates_since_draw: 0,
            error: String::new(),
            hovered: None,
            pressed: None,
            notification: 0,
            published_generation: 0,
            observed_generation: 0,
            outbox: VecDeque::new(),
        }
    }

    fn selected_agent(&self) -> Option<&str> {
        self.agents.get(self.selected).map(String::as_str)
    }
}

enum RoleState {
    Unknown,
    Root(RootState),
    Agent(Box<AgentState>),
    Ui(Box<UiState>),
}

static STATE: Mutex<RoleState> = Mutex::new(RoleState::Unknown);

struct CodingAgentPlugin;

impl Guest for CodingAgentPlugin {
    fn render(_request: brood_plugin_sdk::exports::brood::plugin::actor::PureRenderRequest) -> Result<brood_plugin_sdk::exports::brood::plugin::actor::PureRenderResult, brood_plugin_sdk::exports::brood::plugin::actor::PureRenderError> { Err(brood_plugin_sdk::exports::brood::plugin::actor::PureRenderError::Unsupported) }
    fn dispatch(event: Event) -> Vec<ActorCommand> {
        match event {
            Event::Lifecycle(event) => handle_lifecycle(event),
            Event::Child(event) => handle_child(event),
            Event::Presentation(event) => handle_presentation(event),
            Event::Mailbox(message) => handle_mailbox(&message.sender, &message.body),
            Event::Notification(event) => {
                handle_notification(event.notification, event.generation, event.token)
            }
            Event::Worker(event) => handle_worker_event(event),
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        let request: TurnRequest =
            serde_json::from_slice(&payload).map_err(|error| format!("decode turn: {error}"))?;
        let images = active_worker_images(&request.agent);
        run_brood_agent_turn(&request, &images).await
    }
}

fn active_worker_images(agent: &str) -> Vec<ImageAttachment> {
    let Ok(state) = STATE.lock() else {
        return Vec::new();
    };
    let RoleState::Agent(state) = &*state else {
        return Vec::new();
    };
    if state.id == agent {
        state.active_images.clone()
    } else {
        Vec::new()
    }
}

fn handle_lifecycle(event: LifecycleEvent) -> Vec<ActorCommand> {
    match event {
        LifecycleEvent::Start(context) if context.instance_id == CLIENT_INIT_ID => {
            let backend_id = context.backend_id.unwrap_or_default();
            let session_id = context.session_id.unwrap_or_default();
            set_root_state(true, backend_id, session_id);
            vec![
                start_ui_command(UI_ID, backend_id, session_id),
                ActorCommand::LifecycleComplete(Ok(())),
            ]
        }
        LifecycleEvent::Start(context) if context.instance_id.starts_with("coding-agent-ui") => {
            let mut ui = UiState::new();
            ui.backend_id = context.backend_id.unwrap_or_default();
            ui.session_id = context.session_id.unwrap_or_default();
            if let Ok(mut state) = STATE.lock() {
                *state = RoleState::Ui(Box::new(ui));
            }
            vec![ActorCommand::LifecycleComplete(Ok(()))]
        }
        LifecycleEvent::Start(context) if context.instance_id.contains(".coding-agent-") => {
            if let Ok(mut state) = STATE.lock() {
                *state = RoleState::Agent(Box::new(AgentState::new(
                    context.session_id.unwrap_or_default(),
                )));
            }
            vec![ActorCommand::LifecycleComplete(Ok(()))]
        }
        LifecycleEvent::Start(context) => {
            let session_id = context.session_id.unwrap_or_default();
            set_root_state(false, 0, session_id);
            vec![
                start_agent_command(FIRST_AGENT),
                ActorCommand::LifecycleComplete(Ok(())),
            ]
        }
        LifecycleEvent::Stop(_) => vec![ActorCommand::LifecycleComplete(Ok(()))],
    }
}

fn set_root_state(client: bool, backend_id: u64, session_id: u64) {
    let Ok(mut state) = STATE.lock() else {
        return;
    };
    *state = RoleState::Root(RootState {
        client,
        backend_id,
        session_id,
        agents: Vec::new(),
        subscribers: Vec::new(),
        next_agent: 2,
        next_ui: 2,
    });
}

fn start_ui_command(id: &str, backend_id: u64, session_id: u64) -> ActorCommand {
    start_session_actor(id, COMPONENT_FILE, Vec::new(), backend_id, session_id)
}

fn start_agent_command(id: &str) -> ActorCommand {
    start_global_actor(
        id,
        COMPONENT_FILE,
        vec![
            "filesystem:workspace-write".to_string(),
            "subprocess:brood-agent".to_string(),
            "computer-use".to_string(),
        ],
    )
}

fn handle_child(event: ChildEvent) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let RoleState::Root(root) = &mut *state else {
        return Vec::new();
    };
    match event {
        ChildEvent::Started(started)
            if root.client
                && (started.id == UI_ID || started.id.starts_with("coding-agent-ui-")) =>
        {
            let message = WireMessage {
                kind: "bind".to_string(),
                backend_id: root.backend_id,
                session_id: root.session_id,
                ..WireMessage::default()
            };
            local_message(&started.id, &message).into_iter().collect()
        }
        ChildEvent::Started(started) if !root.client => {
            if !root.agents.iter().any(|agent| agent.id == started.id) {
                root.agents.push(WireAgent {
                    id: started.id,
                    status: "idle".to_string(),
                    model: "default".to_string(),
                    reasoning: "medium".to_string(),
                });
            }
            registry_commands(root)
        }
        ChildEvent::Stopped(id) if !root.client => {
            root.agents.retain(|agent| agent.id != id);
            registry_commands(root)
        }
        ChildEvent::Failed(failed) if !root.client => {
            if let Some(agent) = root.agents.iter_mut().find(|agent| agent.id == failed.id) {
                agent.status = format!("failed: {}", failed.message);
            }
            registry_commands(root)
        }
        _ => Vec::new(),
    }
}

fn handle_mailbox(sender: &str, body: &[u8]) -> Vec<ActorCommand> {
    if sender == INBOX_ID && body == FOCUS_MESSAGE {
        let Ok(state) = STATE.lock() else {
            return Vec::new();
        };
        let RoleState::Ui(ui) = &*state else {
            return Vec::new();
        };
        return (ui.pane != 0)
            .then(|| {
                ActorCommand::SendHostMessage(
                    format!("{{\"type\":\"focus-pane\",\"pane\":{}}}", ui.pane).into_bytes(),
                )
            })
            .into_iter()
            .collect();
    }
    let Ok(message) = serde_json::from_slice::<WireMessage>(body) else {
        return Vec::new();
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match &mut *state {
        RoleState::Root(root) => handle_root_message(root, sender, message),
        RoleState::Agent(agent) => handle_agent_message(agent, sender, message),
        RoleState::Ui(ui) => {
            let subscribe = message.kind.clone();
            let agent_log = message.kind == "agent-log";
            apply_ui_message(ui, message);
            ui.updates_since_draw = ui.updates_since_draw.saturating_add(1);
            if !agent_log || ui.updates_since_draw >= 8 {
                redraw_ui(ui);
                ui.updates_since_draw = 0;
            }
            match subscribe.as_str() {
                "bind" => subscribe_commands(ui),
                "registry" => subscribe_agent_commands(ui),
                _ => Vec::new(),
            }
        }
        RoleState::Unknown => {
            let mut agent = AgentState::new(message.session_id);
            let commands = handle_agent_message(&mut agent, sender, message);
            *state = RoleState::Agent(Box::new(agent));
            commands
        }
    }
}

fn handle_root_message(
    root: &mut RootState,
    sender: &str,
    message: WireMessage,
) -> Vec<ActorCommand> {
    if root.client {
        return match (message.kind.as_str(), message.action.as_str()) {
            ("coding-agent-action", "new-ui") if sender == "key-policy" => {
                let id = format!("coding-agent-ui-{}", root.next_ui);
                root.next_ui = root.next_ui.saturating_add(1);
                vec![start_ui_command(&id, root.backend_id, root.session_id)]
            }
            _ => Vec::new(),
        };
    }
    match message.kind.as_str() {
        "registry-subscribe" => {
            let subscriber = subscriber_from_sender(sender);
            if !root.subscribers.contains(&subscriber) {
                root.subscribers.push(subscriber);
            }
            registry_commands(root)
        }
        "create-agent" => {
            let id = format!("coding-agent-{}", root.next_agent);
            root.next_agent = root.next_agent.saturating_add(1);
            vec![start_agent_command(&id)]
        }
        _ => Vec::new(),
    }
}

fn registry_commands(root: &RootState) -> Vec<ActorCommand> {
    let message = WireMessage {
        kind: "registry".to_string(),
        agents: root.agents.clone(),
        ..WireMessage::default()
    };
    root.subscribers
        .iter()
        .filter_map(|subscriber| subscriber_message(subscriber, &message))
        .collect()
}

fn handle_agent_message(
    agent: &mut AgentState,
    sender: &str,
    message: WireMessage,
) -> Vec<ActorCommand> {
    let subscriber = subscriber_from_sender(sender);
    match message.kind.as_str() {
        "subscribe" => {
            if agent.notification == 0 {
                let Ok(notification) = host::notification_create() else {
                    return Vec::new();
                };
                agent.notification = notification;
                let _ = host::notification_watch_actor(
                    notification,
                    agent.observed_generation,
                    UPDATE_NOTIFICATION_TOKEN,
                );
            }
            if agent.id.is_empty() {
                agent.id = message.agent;
            }
            if !agent.subscribers.contains(&subscriber) {
                agent.subscribers.push(subscriber.clone());
            }
            if agent.active_worker.is_some()
                && agent.active_target.is_none()
                && let Subscriber::Presentation(actor) = &subscriber
                && actor != CLIENT_INIT_ID
            {
                agent.active_target = Some(actor.clone());
            }
            agent_commands(
                agent,
                &WireMessage {
                    kind: "agent-state".to_string(),
                    agent: agent.id.clone(),
                    model: agent.model.clone(),
                    reasoning: agent.reasoning.clone(),
                    status: agent.status.clone(),
                    ..WireMessage::default()
                },
            )
        }
        "image-start" if agent.active_worker.is_none() => {
            let result = begin_agent_image(agent, &message);
            image_transfer_result(agent, result)
        }
        "image-chunk" if agent.active_worker.is_none() => {
            let result = append_agent_image(agent, &message);
            image_transfer_result(agent, result)
        }
        "image-finish" if agent.active_worker.is_none() => {
            let result = finish_agent_image(agent, message.image_id);
            image_transfer_result(agent, result)
        }
        "prompt" if agent.active_worker.is_none() => {
            if let Some(image_id) = message
                .image_ids
                .iter()
                .find(|image_id| !agent.draft_images.contains_key(image_id))
            {
                return agent_commands(
                    agent,
                    &WireMessage {
                        kind: "agent-state".to_string(),
                        agent: agent.id.clone(),
                        status: agent.status.clone(),
                        log_kind: "error".to_string(),
                        text: format!(
                            "Image transfer {image_id} was incomplete; the turn was not started"
                        ),
                        ..WireMessage::default()
                    },
                );
            }
            let mut images = Vec::new();
            for image_id in &message.image_ids {
                let image = agent
                    .draft_images
                    .remove(image_id)
                    .expect("image IDs were checked before removal");
                images.push(image);
            }
            agent.id = message.agent;
            agent.model = message.model;
            agent.reasoning = message.reasoning;
            agent.status = "running".to_string();
            agent.active_worker = Some(WORKER_ID);
            agent.active_images = images;
            agent.active_target = sender
                .strip_prefix("presentation:")
                .filter(|actor| *actor != CLIENT_INIT_ID)
                .map(str::to_string);
            let request = TurnRequest {
                agent: agent.id.clone(),
                prompt: message.prompt,
                model: agent.model.clone(),
                reasoning: agent.reasoning.clone(),
                session_id: agent.session_id,
                history: agent.history.clone(),
            };
            let mut commands = agent_commands(
                agent,
                &WireMessage {
                    kind: "agent-state".to_string(),
                    agent: agent.id.clone(),
                    model: agent.model.clone(),
                    reasoning: agent.reasoning.clone(),
                    status: agent.status.clone(),
                    log_kind: "user".to_string(),
                    text: if agent.active_images.is_empty() {
                        request.prompt.clone()
                    } else {
                        format!(
                            "{} · {} image{}",
                            request.prompt,
                            agent.active_images.len(),
                            if agent.active_images.len() == 1 {
                                ""
                            } else {
                                "s"
                            }
                        )
                    },
                    ..WireMessage::default()
                },
            );
            if let Ok(payload) = serde_json::to_vec(&request) {
                commands.push(ActorCommand::SpawnWorker(WorkerRequest {
                    worker: WORKER_ID,
                    payload,
                }));
            }
            commands
        }
        "configure" => {
            if !message.model.is_empty() {
                agent.model = message.model;
            }
            if !message.reasoning.is_empty() {
                agent.reasoning = message.reasoning;
            }
            agent_commands(
                agent,
                &WireMessage {
                    kind: "agent-state".to_string(),
                    agent: agent.id.clone(),
                    model: agent.model.clone(),
                    reasoning: agent.reasoning.clone(),
                    status: agent.status.clone(),
                    ..WireMessage::default()
                },
            )
        }
        "cancel" if agent.active_worker.is_some() => {
            agent.status = "cancelling".to_string();
            let mut commands = agent_commands(
                agent,
                &WireMessage {
                    kind: "agent-state".to_string(),
                    agent: agent.id.clone(),
                    status: agent.status.clone(),
                    ..WireMessage::default()
                },
            );
            commands.push(ActorCommand::CancelWorker(WORKER_ID));
            commands
        }
        _ => Vec::new(),
    }
}

fn begin_agent_image(agent: &mut AgentState, message: &WireMessage) -> Result<(), String> {
    let total_bytes = usize::try_from(message.total_bytes)
        .map_err(|_| "Image size does not fit this backend".to_string())?;
    if total_bytes == 0 || total_bytes > MAX_IMAGE_BYTES {
        return Err(format!(
            "Image must contain at most {} MiB",
            MAX_IMAGE_BYTES / 1024 / 1024
        ));
    }
    if !matches!(
        message.mime_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) {
        return Err("Unsupported image MIME type".to_string());
    }
    if agent.incoming_images.contains_key(&message.image_id)
        || agent.draft_images.contains_key(&message.image_id)
    {
        return Err("Duplicate image transfer identifier".to_string());
    }
    if agent.incoming_images.len() + agent.draft_images.len() >= MAX_IMAGES {
        return Err(format!("At most {MAX_IMAGES} images can be staged"));
    }
    let aggregate = agent
        .incoming_images
        .values()
        .map(|image| image.total_bytes)
        .chain(agent.draft_images.values().map(|image| image.bytes.len()))
        .sum::<usize>()
        .saturating_add(total_bytes);
    if aggregate > MAX_TOTAL_IMAGE_BYTES {
        return Err(format!(
            "Staged images exceed the {} MiB turn limit",
            MAX_TOTAL_IMAGE_BYTES / 1024 / 1024
        ));
    }
    agent.incoming_images.insert(
        message.image_id,
        IncomingImage {
            name: one_line(&message.image_name, 80),
            mime_type: message.mime_type.clone(),
            total_bytes,
            bytes: Vec::with_capacity(total_bytes),
        },
    );
    Ok(())
}

fn append_agent_image(agent: &mut AgentState, message: &WireMessage) -> Result<(), String> {
    let image = agent
        .incoming_images
        .get_mut(&message.image_id)
        .ok_or_else(|| "Image chunk arrived before its transfer start".to_string())?;
    if message.offset != image.bytes.len() as u64 {
        return Err("Image chunks arrived out of order".to_string());
    }
    let bytes =
        hex_decode(&message.data).map_err(|_| "Image chunk is not valid hex".to_string())?;
    if bytes.len() > IMAGE_CHUNK_BYTES
        || image.bytes.len().saturating_add(bytes.len()) > image.total_bytes
    {
        return Err("Image chunk exceeds its declared transfer bound".to_string());
    }
    image.bytes.extend(bytes);
    Ok(())
}

fn finish_agent_image(agent: &mut AgentState, image_id: u64) -> Result<(), String> {
    let image = agent
        .incoming_images
        .remove(&image_id)
        .ok_or_else(|| "Image finish arrived before its transfer start".to_string())?;
    if image.bytes.len() != image.total_bytes {
        return Err("Image transfer finished before all bytes arrived".to_string());
    }
    validate_image(&image.mime_type, &image.bytes)?;
    agent.draft_images.insert(
        image_id,
        ImageAttachment {
            id: image_id,
            name: image.name,
            mime_type: image.mime_type,
            bytes: image.bytes,
        },
    );
    Ok(())
}

fn image_transfer_result(agent: &AgentState, result: Result<(), String>) -> Vec<ActorCommand> {
    result.map_or_else(
        |text| {
            agent_commands(
                agent,
                &WireMessage {
                    kind: "agent-state".to_string(),
                    agent: agent.id.clone(),
                    status: agent.status.clone(),
                    log_kind: "error".to_string(),
                    text,
                    ..WireMessage::default()
                },
            )
        },
        |()| Vec::new(),
    )
}

fn handle_notification(notification: u64, generation: u64, token: u64) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match &mut *state {
        RoleState::Agent(agent)
            if notification == agent.notification && token == UPDATE_NOTIFICATION_TOKEN =>
        {
            let mut commands = Vec::new();
            for _ in 0..16 {
                let Some((generation, message)) = agent.queued.pop_front() else {
                    break;
                };
                agent.observed_generation = generation;
                commands.extend(agent_commands(agent, &message));
            }
            let _ = host::notification_watch_actor(
                agent.notification,
                agent.observed_generation,
                UPDATE_NOTIFICATION_TOKEN,
            );
            commands
        }
        RoleState::Ui(ui)
            if notification == ui.notification && token == UPDATE_NOTIFICATION_TOKEN =>
        {
            ui.observed_generation = generation;
            let mut commands = Vec::new();
            for _ in 0..32 {
                let Some((agent, message)) = ui.outbox.pop_front() else {
                    break;
                };
                if let Some(command) =
                    session_message(ui.backend_id, ui.session_id, &agent, &message)
                {
                    commands.push(command);
                }
            }
            if !ui.outbox.is_empty() {
                ui.published_generation = ui.published_generation.saturating_add(1);
                let _ = host::notification_signal(ui.notification, ui.published_generation);
            }
            let _ = host::notification_watch_actor(
                ui.notification,
                ui.observed_generation,
                UPDATE_NOTIFICATION_TOKEN,
            );
            commands
        }
        _ => Vec::new(),
    }
}

fn handle_worker_event(event: WorkerEvent) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let RoleState::Agent(agent) = &mut *state else {
        return Vec::new();
    };
    match event {
        WorkerEvent::Finished(finished) if finished.worker == WORKER_ID => {
            if let Ok(history) = serde_json::from_slice::<Vec<HistoryTurn>>(&finished.output) {
                agent.history = history;
            }
            agent.active_worker = None;
            agent.active_images.clear();
            agent.status = "idle".to_string();
            let mut commands = agent_commands(
                agent,
                &WireMessage {
                    kind: "agent-state".to_string(),
                    agent: agent.id.clone(),
                    status: agent.status.clone(),
                    ..WireMessage::default()
                },
            );
            commands.extend(agent_inbox_command(agent, false, "Agent finished"));
            agent.active_target = None;
            commands
        }
        WorkerEvent::Failed(failed) if failed.worker == WORKER_ID => {
            agent.active_worker = None;
            agent.active_images.clear();
            agent.status = "failed".to_string();
            let mut commands = agent_commands(
                agent,
                &WireMessage {
                    kind: "agent-state".to_string(),
                    agent: agent.id.clone(),
                    status: agent.status.clone(),
                    log_kind: "error".to_string(),
                    text: failed.message,
                    ..WireMessage::default()
                },
            );
            commands.extend(agent_inbox_command(agent, true, "Agent failed"));
            agent.active_target = None;
            commands
        }
        WorkerEvent::Cancelled(worker) if worker == WORKER_ID => {
            agent.active_worker = None;
            agent.active_images.clear();
            agent.status = "idle".to_string();
            agent.active_target = None;
            agent_commands(
                agent,
                &WireMessage {
                    kind: "agent-state".to_string(),
                    agent: agent.id.clone(),
                    status: agent.status.clone(),
                    log_kind: "status".to_string(),
                    text: "Turn cancelled".to_string(),
                    ..WireMessage::default()
                },
            )
        }
        _ => Vec::new(),
    }
}

fn agent_inbox_command(agent: &AgentState, failed: bool, title: &str) -> Option<ActorCommand> {
    let target_actor = agent.active_target.as_deref()?;
    presentation_message(
        INBOX_ID,
        &WireMessage {
            kind: "inbox-push".to_string(),
            category: "agent".to_string(),
            title: title.to_string(),
            detail: agent.id.clone(),
            target_actor: target_actor.to_string(),
            status: if failed { "failed" } else { "completed" }.to_string(),
            ..WireMessage::default()
        },
    )
}

fn agent_commands(agent: &AgentState, message: &WireMessage) -> Vec<ActorCommand> {
    agent
        .subscribers
        .iter()
        .filter_map(|subscriber| subscriber_message(subscriber, message))
        .collect()
}

fn subscriber_from_sender(sender: &str) -> Subscriber {
    sender.strip_prefix("presentation:").map_or_else(
        || {
            Subscriber::Local(
                sender
                    .strip_prefix("session:")
                    .unwrap_or(sender)
                    .to_string(),
            )
        },
        |actor| Subscriber::Presentation(actor.to_string()),
    )
}

fn subscriber_message(subscriber: &Subscriber, message: &WireMessage) -> Option<ActorCommand> {
    match subscriber {
        Subscriber::Local(actor) => local_message(actor, message),
        Subscriber::Presentation(actor) => presentation_message(actor, message),
    }
}

fn local_message(actor: &str, message: &WireMessage) -> Option<ActorCommand> {
    serde_json::to_vec(message)
        .ok()
        .map(|body| send_local(actor, body))
}

fn presentation_message(actor: &str, message: &WireMessage) -> Option<ActorCommand> {
    serde_json::to_vec(message)
        .ok()
        .map(|body| send_presentation(actor, body))
}

fn session_message(
    backend_id: u64,
    session_id: u64,
    actor: &str,
    message: &WireMessage,
) -> Option<ActorCommand> {
    serde_json::to_vec(message)
        .ok()
        .map(|body| send_session(backend_id, session_id, actor, body))
}

fn handle_presentation(event: PresentationEvent) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if matches!(*state, RoleState::Unknown) {
        *state = RoleState::Ui(Box::new(UiState::new()));
    }
    let RoleState::Ui(ui) = &mut *state else {
        return Vec::new();
    };
    let (commands, should_redraw) = match event {
        PresentationEvent::PaneCreated(created) => {
            ui.pane = created.pane;
            ui.width = created.width.max(480.0);
            ui.height = created.height.max(360.0);
            let mut commands = subscribe_commands(ui);
            commands.push(ActorCommand::SendHostMessage(
                format!("{{\"type\":\"focus-pane\",\"pane\":{}}}", ui.pane).into_bytes(),
            ));
            (commands, true)
        }
        PresentationEvent::PaneResized(resized) if resized.pane == ui.pane => {
            ui.width = resized.width.max(480.0);
            ui.height = resized.height.max(360.0);
            ui.hovered = None;
            ui.pressed = None;
            ui.scrollbar_drag = None;
            ui.tab_drag = None;
            redraw_ui_for_resize(ui, resized.resize_epoch);
            return Vec::new();
        }
        PresentationEvent::PaneFocused(focused) if focused.pane == ui.pane => {
            ui.pane_focused = focused.focused;
            if !focused.focused {
                ui.hovered = None;
                ui.pressed = None;
                ui.scrollbar_drag = None;
                ui.tab_drag = None;
            }
            (Vec::new(), true)
        }
        PresentationEvent::Key(key) if key.pane == ui.pane => {
            let should_redraw = matches!(
                key.logical_key.as_str(),
                "Backspace"
                    | "Enter"
                    | "Return"
                    | "NumpadEnter"
                    | "Tab"
                    | "F2"
                    | "F6"
                    | "F7"
                    | "Escape"
                    | "ArrowLeft"
                    | "ArrowRight"
                    | "ArrowUp"
                    | "ArrowDown"
                    | "Home"
                    | "End"
                    | "Delete"
                    | "PageUp"
                    | "PageDown"
            ) || (key.modifiers & MOD_CTRL != 0
                && (key.logical_key.eq_ignore_ascii_case("f")
                    || key.logical_key.eq_ignore_ascii_case("a")));
            (
                handle_ui_key(ui, &key.logical_key, key.pressed, key.modifiers),
                should_redraw,
            )
        }
        PresentationEvent::Text(text) if text.pane == ui.pane => {
            if ui
                .suppressed_text
                .take()
                .is_some_and(|key| key.eq_ignore_ascii_case(&text.text))
            {
                return Vec::new();
            }
            insert_ui_text(ui, &text.text);
            (Vec::new(), true)
        }
        PresentationEvent::Paste(paste) if paste.pane == ui.pane => {
            insert_ui_text(ui, &paste.text);
            (Vec::new(), true)
        }
        PresentationEvent::Image(image) if image.pane == ui.pane => {
            if let Err(error) = attach_ui_image(
                ui,
                image.name,
                image.mime_type,
                image.bytes,
                image.preview_width,
                image.preview_height,
                image.preview_rgba8,
            ) {
                ui.error = error;
            }
            (Vec::new(), true)
        }
        PresentationEvent::PointerMoved(moved) if moved.pane == ui.pane => {
            let hovered = ui_target_at(ui, moved.x, moved.y);
            let mut changed = ui.hovered != hovered;
            ui.hovered = hovered;
            if let Some(drag) = ui.scrollbar_drag {
                changed |= set_scroll_from_pointer(ui, moved.y, drag.pointer_offset);
            } else if ui.tab_drag.is_some() {
                changed |= update_tab_drag(ui, moved.x);
            }
            let scrollbar_hovered = scrollbar_hit(ui, moved.x, moved.y);
            changed |= ui.scrollbar_hovered != scrollbar_hovered;
            ui.scrollbar_hovered = scrollbar_hovered;
            (Vec::new(), changed)
        }
        PresentationEvent::PointerLeft(left) if left.pane == ui.pane => {
            let changed = ui.hovered.take().is_some()
                || ui.pressed.take().is_some()
                || ui.scrollbar_drag.take().is_some()
                || ui.tab_drag.take().is_some()
                || ui.scrollbar_hovered;
            ui.scrollbar_hovered = false;
            (Vec::new(), changed)
        }
        PresentationEvent::PointerButton(button)
            if button.pane == ui.pane && button.button == LEFT_BUTTON =>
        {
            let target = ui_target_at(ui, button.x, button.y);
            if button.pressed {
                if begin_scrollbar_drag(ui, button.x, button.y) {
                    (Vec::new(), true)
                } else {
                    ui.hovered.clone_from(&target);
                    ui.pressed.clone_from(&target);
                    if let Some(UiTarget::AgentTab(index)) = target
                        && let Some(agent) = ui.agents.get(index).cloned()
                    {
                        ui.tab_drag = Some(TabDrag {
                            agent,
                            start_x: button.x,
                            moved: false,
                        });
                    }
                    (Vec::new(), true)
                }
            } else {
                if ui.scrollbar_drag.take().is_some() {
                    ui.scrollbar_hovered = scrollbar_hit(ui, button.x, button.y);
                    (Vec::new(), true)
                } else {
                    let pressed = ui.pressed.take();
                    ui.hovered.clone_from(&target);
                    let tab_was_dragged = ui.tab_drag.take().is_some_and(|drag| drag.moved);
                    let commands = match (pressed, target, tab_was_dragged) {
                        (Some(UiTarget::Prompt), Some(UiTarget::Prompt), false) => {
                            set_prompt_cursor_from_pointer(ui, button.x, button.y);
                            Vec::new()
                        }
                        (Some(pressed), Some(released), false) if pressed == released => {
                            activate_ui_target(ui, pressed)
                        }
                        _ => Vec::new(),
                    };
                    (commands, true)
                }
            }
        }
        PresentationEvent::PointerScroll(scroll) if scroll.pane == ui.pane => {
            let changed = scroll_transcript(ui, -scroll.vertical);
            (Vec::new(), changed)
        }
        PresentationEvent::RenderHint(hint) => {
            if let Err(error) = brood_plugin_sdk::graphics::render_with_hint(&hint, || {
                redraw_ui_for_resize(ui, None);
                Ok(())
            }) {
                ui.error = error;
            }
            return Vec::new();
        }
        _ => (Vec::new(), false),
    };
    if should_redraw {
        redraw_ui(ui);
    }
    commands
}

fn apply_ui_message(ui: &mut UiState, message: WireMessage) {
    match message.kind.as_str() {
        "bind" => {
            ui.backend_id = message.backend_id;
            ui.session_id = message.session_id;
            if !message.agent.is_empty() {
                // The picker already established this identity. Seed the UI from
                // the bind so its first frame is useful even before the session
                // registry reply completes the normal reconciliation handshake.
                let agent = message.agent;
                ui.preferred_agent = Some(agent.clone());
                if !ui.agents.contains(&agent) {
                    ui.agents.push(agent.clone());
                }
                let selected = ui
                    .agents
                    .iter()
                    .position(|candidate| candidate == &agent)
                    .unwrap_or_default();
                ui.views.entry(agent).or_insert_with(|| AgentView {
                    status: "connecting".to_string(),
                    model: "default".to_string(),
                    reasoning: "medium".to_string(),
                    logs: VecDeque::new(),
                });
                ui.selected = selected;
            }
        }
        "registry" => {
            if message.session_id != 0 && message.session_id != ui.session_id {
                return;
            }
            let selected = ui
                .preferred_agent
                .take()
                .or_else(|| ui.selected_agent().map(str::to_string));
            let available = message
                .agents
                .iter()
                .map(|agent| agent.id.clone())
                .collect::<Vec<_>>();
            // A registry refresh reconciles membership without undoing a
            // presentation-local drag order.
            ui.agents.retain(|agent| available.contains(agent));
            for agent in &available {
                if !ui.agents.contains(agent) {
                    ui.agents.push(agent.clone());
                }
            }
            for agent in &message.agents {
                let view = ui
                    .views
                    .entry(agent.id.clone())
                    .or_insert_with(|| AgentView::from_wire(agent));
                view.status.clone_from(&agent.status);
                view.model.clone_from(&agent.model);
                view.reasoning.clone_from(&agent.reasoning);
            }
            ui.views.retain(|id, _view| ui.agents.contains(id));
            ui.selected = selected
                .and_then(|selected| ui.agents.iter().position(|id| *id == selected))
                .unwrap_or(0)
                .min(ui.agents.len().saturating_sub(1));
        }
        "agent-state" => {
            let selected = ui.selected_agent() == Some(message.agent.as_str());
            let view = ui
                .views
                .entry(message.agent.clone())
                .or_insert_with(|| AgentView {
                    status: "idle".to_string(),
                    model: "default".to_string(),
                    reasoning: "medium".to_string(),
                    logs: VecDeque::new(),
                });
            if !message.status.is_empty() {
                view.status = message.status;
            }
            if !message.model.is_empty() {
                view.model = message.model;
            }
            if !message.reasoning.is_empty() {
                view.reasoning = message.reasoning;
            }
            if !message.text.is_empty() {
                push_log(view, message.log_kind, message.text);
                if selected && ui.transcript_follow_tail {
                    ui.search_reveal_pending = false;
                }
            }
        }
        "agent-log" => {
            if let Some(view) = ui.views.get_mut(&message.agent) {
                push_log(view, message.log_kind, message.text);
                if ui.transcript_follow_tail {
                    ui.search_reveal_pending = false;
                }
            }
        }
        _ => {}
    }
}

fn push_log(view: &mut AgentView, kind: String, text: String) {
    view.logs.push_back(LogLine { kind, text });
    while view.logs.len() > MAX_LOG_LINES {
        view.logs.pop_front();
    }
}

fn subscribe_commands(ui: &UiState) -> Vec<ActorCommand> {
    if ui.session_id == 0 {
        return Vec::new();
    }
    let mut commands = Vec::new();
    if let Some(command) = session_message(
        ui.backend_id,
        ui.session_id,
        "init",
        &WireMessage {
            kind: "registry-subscribe".to_string(),
            ..WireMessage::default()
        },
    ) {
        commands.push(command);
    }
    commands.extend(subscribe_agent_commands(ui));
    commands
}

fn subscribe_agent_commands(ui: &UiState) -> Vec<ActorCommand> {
    let mut commands = Vec::new();
    for agent in &ui.agents {
        if let Some(command) = subscribe_agent(ui.backend_id, ui.session_id, agent) {
            commands.push(command);
        }
    }
    commands
}

fn subscribe_agent(backend_id: u64, session_id: u64, agent: &str) -> Option<ActorCommand> {
    session_message(
        backend_id,
        session_id,
        agent,
        &WireMessage {
            kind: "subscribe".to_string(),
            agent: agent.to_string(),
            ..WireMessage::default()
        },
    )
}

fn handle_ui_key(ui: &mut UiState, key: &str, pressed: bool, modifiers: u32) -> Vec<ActorCommand> {
    let paste_shortcut = modifiers & MOD_CTRL != 0 && key.eq_ignore_ascii_case("v");
    if paste_shortcut {
        if pressed {
            ui.paste_key_pressed = true;
            ui.suppressed_text = Some(key.to_string());
            return vec![request_clipboard_paste_command(ui.pane)];
        }
        if ui.paste_key_pressed {
            ui.paste_key_pressed = false;
            return Vec::new();
        }
        return vec![request_clipboard_paste_command(ui.pane)];
    }
    if modifiers & MOD_CTRL != 0 && key.eq_ignore_ascii_case("f") {
        if pressed {
            ui.search_active = true;
            ui.search_cursor = ui.search_query.len();
            ui.search_reveal_pending = true;
            ui.suppressed_text = Some(key.to_string());
        }
        return Vec::new();
    }
    if modifiers & MOD_CTRL != 0 && key.eq_ignore_ascii_case("a") {
        if pressed {
            let length = if ui.search_active {
                ui.search_query.len()
            } else {
                ui.prompt.len()
            };
            if ui.search_active {
                ui.search_cursor = length;
            } else {
                ui.prompt_selection = Some((0, length));
                ui.prompt_cursor = length;
            }
            ui.suppressed_text = Some(key.to_string());
        }
        return Vec::new();
    }
    if pressed {
        match key {
            "Backspace" if ui.search_active => {
                delete_before_cursor(&mut ui.search_query, &mut ui.search_cursor);
                ui.search_reveal_pending = true;
            }
            "Backspace" => {
                let edited = delete_prompt_selection(ui)
                    || delete_before_cursor(&mut ui.prompt, &mut ui.prompt_cursor);
                if edited {
                    reset_prompt_history_navigation(ui);
                } else if ui.prompt.is_empty()
                    && let Some(image) = ui.images.pop()
                {
                    retire_image_preview(ui, image.id);
                }
            }
            "Delete" if ui.search_active => {
                delete_after_cursor(&mut ui.search_query, ui.search_cursor);
                ui.search_reveal_pending = true;
            }
            "Delete" => {
                let edited = delete_prompt_selection(ui)
                    || delete_after_cursor(&mut ui.prompt, ui.prompt_cursor);
                if edited {
                    reset_prompt_history_navigation(ui);
                }
            }
            "ArrowLeft" => move_editor_cursor(ui, false, modifiers & MOD_SHIFT != 0),
            "ArrowRight" => move_editor_cursor(ui, true, modifiers & MOD_SHIFT != 0),
            "Home" if ui.search_active => ui.search_cursor = 0,
            "End" if ui.search_active => ui.search_cursor = ui.search_query.len(),
            "Home" => set_prompt_cursor(ui, line_start(&ui.prompt, ui.prompt_cursor), false),
            "End" => set_prompt_cursor(ui, line_end(&ui.prompt, ui.prompt_cursor), false),
            "ArrowUp" if !ui.search_active => {
                if !move_prompt_line(ui, false) && modifiers & MOD_SHIFT == 0 {
                    navigate_prompt_history(ui, false);
                }
            }
            "ArrowDown" if !ui.search_active => {
                if !move_prompt_line(ui, true) && modifiers & MOD_SHIFT == 0 {
                    navigate_prompt_history(ui, true);
                }
            }
            "PageUp" => {
                let amount = -ui.transcript_viewport_height * 0.82;
                scroll_transcript(ui, amount);
            }
            "PageDown" => {
                let amount = ui.transcript_viewport_height * 0.82;
                scroll_transcript(ui, amount);
            }
            _ => {}
        }
        return Vec::new();
    }
    match key {
        "Enter" | "Return" | "NumpadEnter" if ui.search_active => {
            cycle_search_match(ui, modifiers & MOD_SHIFT == 0);
            Vec::new()
        }
        "Enter" | "Return" | "NumpadEnter" if modifiers & MOD_SHIFT != 0 => {
            insert_ui_text(ui, "\n");
            Vec::new()
        }
        "Enter" | "Return" | "NumpadEnter" => submit_prompt(ui),
        "Tab" => {
            if !ui.agents.is_empty() {
                ui.selected = (ui.selected + 1) % ui.agents.len();
                reset_transcript_view(ui);
            }
            Vec::new()
        }
        "F2" => create_agent(ui).into_iter().collect(),
        "F6" => configure_selected(ui, true).into_iter().collect(),
        "F7" => configure_selected(ui, false).into_iter().collect(),
        "Escape" if ui.search_active => {
            ui.search_active = false;
            Vec::new()
        }
        "Escape" => cancel_selected(ui).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn insert_ui_text(ui: &mut UiState, text: &str) {
    let filtered = text
        .chars()
        .filter_map(|character| match character {
            '\r' => None,
            '\t' => Some(' '),
            '\n' => Some('\n'),
            character if character.is_control() => None,
            character => Some(character),
        })
        .collect::<String>();
    if filtered.is_empty() {
        return;
    }
    if ui.search_active {
        ui.search_query.insert_str(ui.search_cursor, &filtered);
        ui.search_cursor += filtered.len();
        ui.search_reveal_pending = true;
        ui.search_match = 0;
    } else {
        delete_prompt_selection(ui);
        ui.prompt.insert_str(ui.prompt_cursor, &filtered);
        ui.prompt_cursor += filtered.len();
        reset_prompt_history_navigation(ui);
    }
}

fn previous_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor.min(text.len())]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[cursor..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(offset, _)| cursor + offset)
}

fn delete_before_cursor(text: &mut String, cursor: &mut usize) -> bool {
    if *cursor == 0 {
        return false;
    }
    let start = previous_boundary(text, *cursor);
    text.replace_range(start..*cursor, "");
    *cursor = start;
    true
}

fn delete_after_cursor(text: &mut String, cursor: usize) -> bool {
    if cursor >= text.len() {
        return false;
    }
    let end = next_boundary(text, cursor);
    text.replace_range(cursor..end, "");
    true
}

fn delete_prompt_selection(ui: &mut UiState) -> bool {
    let Some((first, second)) = ui.prompt_selection.take() else {
        return false;
    };
    let start = first.min(second).min(ui.prompt.len());
    let end = first.max(second).min(ui.prompt.len());
    if start == end {
        return false;
    }
    ui.prompt.replace_range(start..end, "");
    ui.prompt_cursor = start;
    true
}

fn set_prompt_cursor(ui: &mut UiState, cursor: usize, extend: bool) {
    let cursor = cursor.min(ui.prompt.len());
    if extend {
        let anchor = ui
            .prompt_selection
            .map_or(ui.prompt_cursor, |selection| selection.0);
        ui.prompt_selection = Some((anchor, cursor));
    } else {
        ui.prompt_selection = None;
    }
    ui.prompt_cursor = cursor;
}

fn move_editor_cursor(ui: &mut UiState, right: bool, extend: bool) {
    if ui.search_active {
        ui.search_cursor = if right {
            next_boundary(&ui.search_query, ui.search_cursor)
        } else {
            previous_boundary(&ui.search_query, ui.search_cursor)
        };
        return;
    }
    let cursor = if right {
        next_boundary(&ui.prompt, ui.prompt_cursor)
    } else {
        previous_boundary(&ui.prompt, ui.prompt_cursor)
    };
    set_prompt_cursor(ui, cursor, extend);
}

fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor.min(text.len())]
        .rfind('\n')
        .map_or(0, |index| index + 1)
}

fn line_end(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[cursor..]
        .find('\n')
        .map_or(text.len(), |offset| cursor + offset)
}

fn move_prompt_line(ui: &mut UiState, down: bool) -> bool {
    let start = line_start(&ui.prompt, ui.prompt_cursor);
    let column = ui.prompt[start..ui.prompt_cursor].chars().count();
    let target_start = if down {
        let end = line_end(&ui.prompt, ui.prompt_cursor);
        (end < ui.prompt.len()).then_some(end + 1)
    } else if start > 0 {
        Some(line_start(&ui.prompt, start - 1))
    } else {
        None
    };
    let Some(target_start) = target_start else {
        return false;
    };
    let target_end = line_end(&ui.prompt, target_start);
    let target = ui.prompt[target_start..target_end]
        .char_indices()
        .nth(column)
        .map_or(target_end, |(offset, _)| target_start + offset);
    set_prompt_cursor(ui, target, false);
    true
}

fn reset_prompt_history_navigation(ui: &mut UiState) {
    ui.prompt_history_cursor = None;
    ui.prompt_history_draft = None;
}

fn navigate_prompt_history(ui: &mut UiState, forward: bool) -> bool {
    if ui.prompt_history.is_empty() || ui.prompt_selection.is_some() {
        return false;
    }
    let next = match (ui.prompt_history_cursor, forward) {
        (None, true) => return false,
        (None, false) => {
            ui.prompt_history_draft = Some(ui.prompt.clone());
            ui.prompt_history.len() - 1
        }
        (Some(current), false) => current.saturating_sub(1),
        (Some(current), true) if current + 1 < ui.prompt_history.len() => current + 1,
        (Some(_), true) => {
            ui.prompt = ui.prompt_history_draft.take().unwrap_or_default();
            ui.prompt_cursor = ui.prompt.len();
            ui.prompt_selection = None;
            ui.prompt_history_cursor = None;
            return true;
        }
    };
    ui.prompt = ui.prompt_history[next].clone();
    ui.prompt_cursor = ui.prompt.len();
    ui.prompt_selection = None;
    ui.prompt_history_cursor = Some(next);
    true
}

fn record_prompt_history(ui: &mut UiState, prompt: &str) {
    if prompt.trim().is_empty() {
        return;
    }
    if ui
        .prompt_history
        .back()
        .is_none_or(|previous| previous != prompt)
    {
        if ui.prompt_history.len() == MAX_PROMPT_HISTORY {
            ui.prompt_history.pop_front();
        }
        ui.prompt_history.push_back(prompt.to_string());
    }
    reset_prompt_history_navigation(ui);
}

fn set_prompt_cursor_from_pointer(ui: &mut UiState, x: f32, y: f32) {
    ui.search_active = false;
    ui.prompt_selection = None;
    let rect = prompt_rect(ui);
    let editor_width = (rect.width - 122.0).max(120.0);
    let columns = (editor_width / 7.4).max(16.0) as usize;
    let lines = wrap_editor_text(&ui.prompt, columns);
    let current_line = lines
        .iter()
        .position(|line| ui.prompt_cursor >= line.start && ui.prompt_cursor <= line.end)
        .unwrap_or_else(|| lines.len().saturating_sub(1));
    let first_line = current_line.saturating_sub(1);
    let row = (((y - (rect.y + 27.0)) / 20.0).floor() as isize).clamp(0, 1) as usize;
    let Some(line) = lines.get(first_line + row) else {
        ui.prompt_cursor = ui.prompt.len();
        return;
    };
    let column =
        (((x - (rect.x + 13.0)) / 7.4).round() as isize).clamp(0, columns as isize) as usize;
    ui.prompt_cursor = ui.prompt[line.start..line.end]
        .char_indices()
        .nth(column)
        .map_or(line.end, |(offset, _)| line.start + offset);
}

fn request_clipboard_paste_command(pane: u64) -> ActorCommand {
    ActorCommand::SendHostMessage(
        format!(r#"{{"type":"request-clipboard-paste","pane":{pane}}}"#).into_bytes(),
    )
}

fn ui_target_at(ui: &UiState, x: f32, y: f32) -> Option<UiTarget> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    for index in 0..ui.agents.len() {
        if rect_contains(agent_tab_rect(ui, index), x, y) {
            return Some(UiTarget::AgentTab(index));
        }
    }
    if rect_contains(new_agent_rect(ui.width), x, y) {
        return Some(UiTarget::NewAgent);
    }
    if rect_contains(search_button_rect(ui.width), x, y) {
        return Some(UiTarget::Search);
    }
    let (model, reasoning) = toolbar_control_rects(ui.width);
    if ui.selected_agent().is_some() && rect_contains(model, x, y) {
        return Some(UiTarget::Model);
    }
    if ui.selected_agent().is_some() && rect_contains(reasoning, x, y) {
        return Some(UiTarget::Reasoning);
    }
    if can_submit(ui) && rect_contains(send_button_rect(ui), x, y) {
        return Some(UiTarget::Send);
    }
    if rect_contains(prompt_rect(ui), x, y) {
        return Some(UiTarget::Prompt);
    }
    None
}

fn activate_ui_target(ui: &mut UiState, target: UiTarget) -> Vec<ActorCommand> {
    match target {
        UiTarget::AgentTab(index) if index < ui.agents.len() => {
            ui.selected = index;
            reset_transcript_view(ui);
            Vec::new()
        }
        UiTarget::NewAgent => create_agent(ui).into_iter().collect(),
        UiTarget::Search => {
            ui.search_active = !ui.search_active;
            ui.search_cursor = ui.search_query.len();
            Vec::new()
        }
        UiTarget::Model => configure_selected(ui, true).into_iter().collect(),
        UiTarget::Reasoning => configure_selected(ui, false).into_iter().collect(),
        UiTarget::Send => submit_prompt(ui),
        UiTarget::Prompt => {
            ui.search_active = false;
            ui.prompt_cursor = ui.prompt.len();
            ui.prompt_selection = None;
            Vec::new()
        }
        UiTarget::AgentTab(_) => Vec::new(),
    }
}

fn agent_tab_width(ui: &UiState) -> f32 {
    if ui.agents.is_empty() {
        152.0
    } else {
        ((ui.width - 116.0) / ui.agents.len() as f32).clamp(52.0, 144.0)
    }
}

fn agent_tab_rect(ui: &UiState, index: usize) -> Rect {
    let width = agent_tab_width(ui);
    Rect::new(10.0 + index as f32 * (width + 3.0), 7.0, width, 28.0)
}

fn toolbar_control_rects(width: f32) -> (Rect, Rect) {
    let control_width = ((width - 198.0) * 0.5).clamp(92.0, 180.0);
    let reasoning = Rect::new(width - control_width - 10.0, 42.0, control_width, 26.0);
    let model = Rect::new(reasoning.x - control_width - 6.0, 42.0, control_width, 26.0);
    (model, reasoning)
}

fn new_agent_rect(width: f32) -> Rect {
    Rect::new(width - 40.0, 7.0, 30.0, 28.0)
}

fn search_button_rect(width: f32) -> Rect {
    Rect::new(width - 76.0, 7.0, 30.0, 28.0)
}

fn prompt_rect(ui: &UiState) -> Rect {
    Rect::new(
        12.0,
        ui.height - PROMPT_HEIGHT - 12.0,
        ui.width - 24.0,
        PROMPT_HEIGHT,
    )
}

fn send_button_rect(ui: &UiState) -> Rect {
    let prompt = prompt_rect(ui);
    Rect::new(ui.width - 94.0, prompt.y + prompt.height - 38.0, 70.0, 28.0)
}

fn rect_contains(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

fn reset_transcript_view(ui: &mut UiState) {
    ui.transcript_follow_tail = true;
    ui.transcript_scroll = ui.transcript_content_height;
    ui.search_match = 0;
    ui.search_reveal_pending = ui.search_active;
}

fn max_transcript_scroll(ui: &UiState) -> f32 {
    (ui.transcript_content_height - ui.transcript_viewport_height).max(0.0)
}

fn scroll_transcript(ui: &mut UiState, delta: f32) -> bool {
    if !delta.is_finite() || delta == 0.0 {
        return false;
    }
    let maximum = max_transcript_scroll(ui);
    let next = (ui.transcript_scroll + delta).clamp(0.0, maximum);
    if (next - ui.transcript_scroll).abs() < 0.01 {
        return false;
    }
    ui.transcript_scroll = next;
    ui.transcript_follow_tail = next >= maximum - 1.0;
    true
}

fn search_matches(ui: &UiState) -> Vec<usize> {
    let query = if ui.search_active {
        ui.search_query.trim().to_lowercase()
    } else {
        String::new()
    };
    if query.is_empty() {
        return Vec::new();
    }
    ui.selected_agent()
        .and_then(|agent| ui.views.get(agent))
        .map(|view| {
            view.logs
                .iter()
                .enumerate()
                .filter_map(|(index, line)| {
                    line.text.to_lowercase().contains(&query).then_some(index)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cycle_search_match(ui: &mut UiState, forward: bool) {
    let count = search_matches(ui).len();
    if count == 0 {
        ui.search_match = 0;
        return;
    }
    ui.search_match = if forward {
        (ui.search_match + 1) % count
    } else {
        (ui.search_match + count - 1) % count
    };
    ui.search_reveal_pending = true;
}

fn transcript_rect(ui: &UiState) -> Rect {
    let top = HEADER_HEIGHT + TRANSCRIPT_GAP + if ui.search_active { SEARCH_HEIGHT } else { 0.0 };
    let prompt = prompt_rect(ui);
    let image_extra = if ui.images.is_empty() {
        0.0
    } else {
        IMAGE_TRAY_HEIGHT + IMAGE_TRAY_GAP
    };
    Rect::new(
        10.0,
        top,
        ui.width - 20.0,
        (prompt.y - top - TRANSCRIPT_GAP - image_extra).max(48.0),
    )
}

fn scrollbar_geometry(ui: &UiState) -> Option<(f32, f32, f32, f32)> {
    let viewport = transcript_rect(ui);
    if ui.transcript_content_height <= viewport.height + 0.5 {
        return None;
    }
    let track_top = viewport.y + 6.0;
    let track_height = (viewport.height - 12.0).max(1.0);
    let thumb_height = (track_height * viewport.height / ui.transcript_content_height)
        .max(28.0)
        .min(track_height);
    let travel = (track_height - thumb_height).max(0.0);
    let thumb_y = track_top + travel * ui.transcript_scroll / max_transcript_scroll(ui).max(1.0);
    Some((track_top, track_height, thumb_y, thumb_height))
}

fn scrollbar_hit(ui: &UiState, x: f32, y: f32) -> bool {
    let Some((track_top, track_height, _, _)) = scrollbar_geometry(ui) else {
        return false;
    };
    x.is_finite()
        && y.is_finite()
        && x >= ui.width - SCROLLBAR_HIT_WIDTH
        && x <= ui.width
        && y >= track_top
        && y <= track_top + track_height
}

fn begin_scrollbar_drag(ui: &mut UiState, x: f32, y: f32) -> bool {
    if !scrollbar_hit(ui, x, y) {
        return false;
    }
    let Some((_, _, thumb_y, thumb_height)) = scrollbar_geometry(ui) else {
        return false;
    };
    let pointer_offset = if y >= thumb_y && y <= thumb_y + thumb_height {
        y - thumb_y
    } else {
        thumb_height * 0.5
    };
    ui.scrollbar_drag = Some(ScrollbarDrag { pointer_offset });
    ui.scrollbar_hovered = true;
    set_scroll_from_pointer(ui, y, pointer_offset);
    true
}

fn set_scroll_from_pointer(ui: &mut UiState, pointer_y: f32, pointer_offset: f32) -> bool {
    if !pointer_y.is_finite() {
        return false;
    }
    let Some((track_top, track_height, _, thumb_height)) = scrollbar_geometry(ui) else {
        return false;
    };
    let travel = (track_height - thumb_height).max(0.0);
    if travel == 0.0 {
        return false;
    }
    let thumb_y = (pointer_y - pointer_offset).clamp(track_top, track_top + travel);
    let next = (thumb_y - track_top) / travel * max_transcript_scroll(ui);
    let changed = (next - ui.transcript_scroll).abs() >= 0.01;
    ui.transcript_scroll = next;
    ui.transcript_follow_tail = next >= max_transcript_scroll(ui) - 1.0;
    changed
}

fn update_tab_drag(ui: &mut UiState, x: f32) -> bool {
    let Some(drag) = ui.tab_drag.as_mut() else {
        return false;
    };
    if !x.is_finite() {
        return false;
    }
    if (x - drag.start_x).abs() > 5.0 {
        drag.moved = true;
    }
    if !drag.moved {
        return false;
    }
    let agent = drag.agent.clone();
    let Some(from) = ui.agents.iter().position(|candidate| candidate == &agent) else {
        return false;
    };
    let selected_agent = ui.selected_agent().map(str::to_string);
    let width = agent_tab_width(ui) + 3.0;
    let to = (((x - 10.0) / width).floor() as isize)
        .clamp(0, ui.agents.len().saturating_sub(1) as isize) as usize;
    if from == to {
        return true;
    }
    let moved = ui.agents.remove(from);
    ui.agents.insert(to, moved);
    ui.selected = selected_agent
        .and_then(|selected| {
            ui.agents
                .iter()
                .position(|candidate| candidate == &selected)
        })
        .unwrap_or(0);
    ui.pressed = Some(UiTarget::AgentTab(to));
    true
}

fn attach_ui_image(
    ui: &mut UiState,
    name: String,
    mime_type: String,
    bytes: Vec<u8>,
    preview_width: u32,
    preview_height: u32,
    preview_rgba8: Vec<u8>,
) -> Result<(), String> {
    if ui.images.len() >= MAX_IMAGES {
        return Err(format!(
            "At most {MAX_IMAGES} images can be attached to one turn"
        ));
    }
    validate_image(&mime_type, &bytes)?;
    let total = ui
        .images
        .iter()
        .map(|image| image.bytes.len())
        .sum::<usize>()
        .saturating_add(bytes.len());
    if total > MAX_TOTAL_IMAGE_BYTES {
        return Err(format!(
            "Attached images exceed the {} MiB turn limit",
            MAX_TOTAL_IMAGE_BYTES / 1024 / 1024
        ));
    }
    let expected_preview_bytes = usize::try_from(preview_width)
        .ok()
        .and_then(|width| {
            usize::try_from(preview_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "Image preview dimensions are invalid".to_string())?;
    if preview_width == 0 || preview_height == 0 || expected_preview_bytes != preview_rgba8.len() {
        return Err("Image preview pixels are invalid".to_string());
    }
    let preview_handle =
        host::graphics_create_image_rgba8(ui.pane, preview_width, preview_height, &preview_rgba8)
            .map_err(|error| format!("Create image preview: {error:?}"))?;
    let id = ui.pane.rotate_left(32) ^ ui.next_image.max(1);
    ui.next_image = ui.next_image.saturating_add(1);
    ui.images.push(ImageAttachment {
        id,
        name: one_line(if name.is_empty() { "image" } else { &name }, 80),
        mime_type,
        bytes,
    });
    ui.image_previews.insert(
        id,
        ImagePreview {
            handle: preview_handle,
            width: preview_width,
            height: preview_height,
        },
    );
    ui.error.clear();
    Ok(())
}

fn retire_image_preview(ui: &mut UiState, image_id: u64) {
    if let Some(preview) = ui.image_previews.remove(&image_id) {
        ui.retired_image_previews.push(preview.handle);
    }
}

fn validate_image(mime_type: &str, bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() {
        return Err("Image is empty".to_string());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "Image exceeds the {} MiB attachment limit",
            MAX_IMAGE_BYTES / 1024 / 1024
        ));
    }
    let detected = image_mime_type(bytes)
        .ok_or_else(|| "Only PNG, JPEG, WebP, and GIF images can be attached".to_string())?;
    if detected != mime_type {
        return Err(format!(
            "Image MIME type {mime_type:?} does not match {detected:?} bytes"
        ));
    }
    Ok(())
}

fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        Some("image/webp")
    } else {
        None
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn hex_decode(encoded: &str) -> Result<Vec<u8>, String> {
    if !encoded.len().is_multiple_of(2) {
        return Err("Image chunk has an odd hexadecimal length".to_string());
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(String::new()),
    }
}

fn can_submit(ui: &UiState) -> bool {
    ui.session_id != 0
        && ui.selected_agent().is_some()
        && (!ui.prompt.trim().is_empty() || !ui.images.is_empty())
}

fn submit_prompt(ui: &mut UiState) -> Vec<ActorCommand> {
    let Some(agent) = ui.selected_agent().map(str::to_string) else {
        return Vec::new();
    };
    if (!can_submit(ui)) || ui.session_id == 0 {
        return Vec::new();
    }
    let Some(view) = ui.views.get(&agent) else {
        return Vec::new();
    };
    let model = view.model.clone();
    let reasoning = view.reasoning.clone();
    let has_typed_prompt = !ui.prompt.trim().is_empty();
    let prompt = if !has_typed_prompt {
        "Describe the attached image(s).".to_string()
    } else {
        std::mem::take(&mut ui.prompt)
    };
    if has_typed_prompt {
        record_prompt_history(ui, &prompt);
    }
    ui.prompt_cursor = 0;
    ui.prompt_selection = None;
    let images = std::mem::take(&mut ui.images);
    let image_ids = images.iter().map(|image| image.id).collect::<Vec<_>>();
    let mut messages = Vec::new();
    for image in &images {
        messages.extend(image_transfer_messages(image));
    }
    for image in &images {
        retire_image_preview(ui, image.id);
    }
    messages.push(WireMessage {
        kind: "prompt".to_string(),
        agent: agent.clone(),
        prompt,
        model,
        reasoning,
        image_ids,
        ..WireMessage::default()
    });
    queue_ui_messages(ui, &agent, messages)
}

fn image_transfer_messages(image: &ImageAttachment) -> Vec<WireMessage> {
    let mut messages = vec![WireMessage {
        kind: "image-start".to_string(),
        image_id: image.id,
        image_name: image.name.clone(),
        mime_type: image.mime_type.clone(),
        total_bytes: image.bytes.len() as u64,
        ..WireMessage::default()
    }];
    for (index, chunk) in image.bytes.chunks(IMAGE_CHUNK_BYTES).enumerate() {
        messages.push(WireMessage {
            kind: "image-chunk".to_string(),
            image_id: image.id,
            offset: (index * IMAGE_CHUNK_BYTES) as u64,
            data: hex_encode(chunk),
            ..WireMessage::default()
        });
    }
    messages.push(WireMessage {
        kind: "image-finish".to_string(),
        image_id: image.id,
        ..WireMessage::default()
    });
    messages
}

fn queue_ui_messages(
    ui: &mut UiState,
    agent: &str,
    messages: Vec<WireMessage>,
) -> Vec<ActorCommand> {
    if ui.notification == 0 {
        let Ok(notification) = host::notification_create() else {
            ui.error = "Could not create the image transfer notification".to_string();
            return Vec::new();
        };
        ui.notification = notification;
        let _ = host::notification_watch_actor(
            notification,
            ui.observed_generation,
            UPDATE_NOTIFICATION_TOKEN,
        );
    }
    ui.outbox.extend(
        messages
            .into_iter()
            .map(|message| (agent.to_string(), message)),
    );
    ui.published_generation = ui.published_generation.saturating_add(1);
    if host::notification_signal(ui.notification, ui.published_generation).is_err() {
        ui.error = "Could not start the image transfer".to_string();
    }
    Vec::new()
}

fn create_agent(ui: &UiState) -> Option<ActorCommand> {
    session_message(
        ui.backend_id,
        ui.session_id,
        "init",
        &WireMessage {
            kind: "create-agent".to_string(),
            ..WireMessage::default()
        },
    )
}

fn configure_selected(ui: &mut UiState, model: bool) -> Option<ActorCommand> {
    let agent = ui.selected_agent()?.to_string();
    let view = ui.views.get_mut(&agent)?;
    if model {
        view.model = match view.model.as_str() {
            "default" => "gpt-5.4",
            "gpt-5.4" => "gpt-5.3-codex",
            _ => "default",
        }
        .to_string();
    } else {
        view.reasoning = match view.reasoning.as_str() {
            "low" => "medium",
            "medium" => "high",
            "high" => "xhigh",
            _ => "low",
        }
        .to_string();
    }
    session_message(
        ui.backend_id,
        ui.session_id,
        &agent,
        &WireMessage {
            kind: "configure".to_string(),
            agent: agent.clone(),
            model: view.model.clone(),
            reasoning: view.reasoning.clone(),
            ..WireMessage::default()
        },
    )
}

fn cancel_selected(ui: &UiState) -> Option<ActorCommand> {
    let agent = ui.selected_agent()?.to_string();
    session_message(
        ui.backend_id,
        ui.session_id,
        &agent,
        &WireMessage {
            kind: "cancel".to_string(),
            agent: agent.clone(),
            ..WireMessage::default()
        },
    )
}

fn redraw_ui(ui: &mut UiState) {
    redraw_ui_for_resize(ui, None);
}

fn redraw_ui_for_resize(ui: &mut UiState, resize_epoch: Option<u64>) {
    if ui.pane == 0 {
        return;
    }
    if !brood_plugin_sdk::graphics::render_is_active() {
        if let Err(error) =
            brood_plugin_sdk::graphics::request_render_for_resize(ui.pane, resize_epoch)
        {
            ui.error = error;
        }
        return;
    }
    let mut text_cache = ui
        .text_cache
        .take()
        .unwrap_or_else(|| TextLayoutCache::new(ui.pane));
    text_cache.set_pane(ui.pane);
    text_cache.begin_frame();
    ui.draw_error = None;
    let mut packet = Packet::new();
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, ui.width, ui.height),
        color: BG,
    });
    draw_agent_tabs(ui, &mut text_cache, &mut packet);
    draw_toolbar(ui, &mut text_cache, &mut packet);
    draw_search(ui, &mut text_cache, &mut packet);
    draw_transcript(ui, &mut text_cache, &mut packet);
    draw_prompt(ui, &mut text_cache, &mut packet);
    let submitted = if let Some(error) = ui.draw_error.take() {
        text_cache.abort_frame();
        Err(error)
    } else {
        text_cache.queue(resize_epoch, &packet).map(|_| ())
    };
    match submitted {
        Ok(()) => {
            drop_resources(ui.pane, ui.retired_image_previews.drain(..));
            ui.error.clear();
        }
        Err(error) => {
            ui.error = error;
        }
    }
    ui.text_cache = Some(text_cache);
}

fn draw_agent_tabs(ui: &mut UiState, text_cache: &mut TextLayoutCache, packet: &mut Packet) {
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, ui.width, 40.0),
        color: SUNKEN,
    });
    let agents = ui.agents.clone();
    for (index, agent) in agents.iter().enumerate() {
        let rect = agent_tab_rect(ui, index);
        let target = UiTarget::AgentTab(index);
        let pressed = ui.pressed.as_ref() == Some(&target);
        let hovered = ui.hovered.as_ref() == Some(&target);
        packet.push(Command::FillRoundedRect {
            rect,
            radius: 4.0,
            color: if pressed {
                PRESSED
            } else if hovered {
                HOVER
            } else if index == ui.selected {
                PANEL_LIGHT
            } else {
                SUNKEN
            },
        });
        if index == ui.selected {
            packet.push(Command::FillRoundedRect {
                rect: Rect::new(
                    rect.x + 7.0,
                    rect.y + rect.height - 3.0,
                    rect.width - 14.0,
                    3.0,
                ),
                radius: 1.5,
                color: BLUE,
            });
        }
        draw_text(
            ui,
            text_cache,
            packet,
            agent,
            rect.x + 7.0,
            if pressed { 12.0 } else { 11.0 },
            13.0,
            (rect.width - 14.0).max(24.0),
            if index == ui.selected { BRIGHT } else { FG },
        );
    }
    draw_header_icon_button(
        ui,
        text_cache,
        packet,
        UiTarget::Search,
        search_button_rect(ui.width),
        "⌕",
    );
    draw_header_icon_button(
        ui,
        text_cache,
        packet,
        UiTarget::NewAgent,
        new_agent_rect(ui.width),
        "+",
    );
}

fn draw_header_icon_button(
    ui: &mut UiState,
    text_cache: &mut TextLayoutCache,
    packet: &mut Packet,
    target: UiTarget,
    rect: Rect,
    label: &str,
) {
    let active = target == UiTarget::Search && ui.search_active;
    let pressed = ui.pressed.as_ref() == Some(&target);
    let hovered = ui.hovered.as_ref() == Some(&target);
    packet.push(Command::FillRoundedRect {
        rect,
        radius: 4.0,
        color: if pressed {
            PRESSED
        } else if hovered || active {
            HOVER
        } else {
            SUNKEN
        },
    });
    draw_text(
        ui,
        text_cache,
        packet,
        label,
        rect.x + 9.0,
        rect.y + 5.0,
        16.0,
        18.0,
        if active { BLUE } else { FG },
    );
}

fn draw_toolbar(ui: &mut UiState, text_cache: &mut TextLayoutCache, packet: &mut Packet) {
    let (status, model, reasoning) = ui
        .selected_agent()
        .and_then(|agent| ui.views.get(agent))
        .map_or_else(
            || {
                (
                    "waiting".to_string(),
                    "default".to_string(),
                    "medium".to_string(),
                )
            },
            |view| {
                (
                    view.status.clone(),
                    view.model.clone(),
                    view.reasoning.clone(),
                )
            },
        );
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 40.0, ui.width, 36.0),
        color: PANEL,
    });
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, HEADER_HEIGHT - 1.0, ui.width, 1.0),
        color: ashen::SUBTLE,
    });
    let (model_rect, reasoning_rect) = toolbar_control_rects(ui.width);
    draw_text(
        ui,
        text_cache,
        packet,
        &format!("●  {status}"),
        12.0,
        49.0,
        12.0,
        (model_rect.x - 22.0).max(70.0),
        if status == "running" {
            GREEN
        } else if status.starts_with("failed") {
            RED
        } else {
            MUTED
        },
    );
    draw_toolbar_button(
        ui,
        text_cache,
        packet,
        UiTarget::Model,
        model_rect,
        &format!("{model}  ·  model"),
        BLUE,
    );
    draw_toolbar_button(
        ui,
        text_cache,
        packet,
        UiTarget::Reasoning,
        reasoning_rect,
        &format!("{reasoning}  ·  effort"),
        YELLOW,
    );
}

fn draw_toolbar_button(
    ui: &mut UiState,
    text_cache: &mut TextLayoutCache,
    packet: &mut Packet,
    target: UiTarget,
    rect: Rect,
    label: &str,
    label_color: Color,
) {
    let pressed = ui.pressed.as_ref() == Some(&target);
    let hovered = ui.hovered.as_ref() == Some(&target);
    packet.push(Command::FillRoundedRect {
        rect,
        radius: 4.0,
        color: if pressed {
            PRESSED
        } else if hovered {
            HOVER
        } else {
            PANEL
        },
    });
    draw_text(
        ui,
        text_cache,
        packet,
        label,
        rect.x + 8.0,
        rect.y + if pressed { 6.0 } else { 5.0 },
        11.0,
        rect.width - 18.0,
        if ui.selected_agent().is_some() {
            label_color
        } else {
            MUTED
        },
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarkdownKind {
    Paragraph,
    Heading,
    Bullet,
    Quote,
    Code,
    Rule,
}

#[derive(Clone, Debug)]
struct MarkdownBlock {
    kind: MarkdownKind,
    text: String,
}

#[derive(Clone, Debug)]
struct PreparedBlock {
    kind: MarkdownKind,
    text: String,
    size: f32,
    color: Color,
    height: f32,
}

#[derive(Clone, Debug)]
struct PreparedCard {
    log_index: usize,
    kind: String,
    blocks: Vec<PreparedBlock>,
    height: f32,
    matched: bool,
}

fn draw_search(ui: &mut UiState, text_cache: &mut TextLayoutCache, packet: &mut Packet) {
    if !ui.search_active {
        return;
    }
    let rect = Rect::new(
        10.0,
        HEADER_HEIGHT + 7.0,
        ui.width - 20.0,
        SEARCH_HEIGHT - 8.0,
    );
    packet.push(Command::FillRoundedRect {
        rect,
        radius: 6.0,
        color: PANEL,
    });
    packet.push(Command::StrokeRoundedRect {
        rect,
        radius: 6.0,
        color: ACCENT_MUTED,
        width: 1.0,
    });
    let matches = search_matches(ui);
    let query_empty = ui.search_query.is_empty();
    let query = if query_empty {
        "Search this conversation".to_string()
    } else {
        ui.search_query.clone()
    };
    draw_text(
        ui,
        text_cache,
        packet,
        &query,
        rect.x + 30.0,
        rect.y + 8.0,
        13.0,
        rect.width - 116.0,
        if query_empty { MUTED } else { FG },
    );
    draw_text(
        ui,
        text_cache,
        packet,
        "⌕",
        rect.x + 10.0,
        rect.y + 7.0,
        15.0,
        16.0,
        BLUE,
    );
    let counter = if matches.is_empty() {
        "0 results".to_string()
    } else {
        format!(
            "{} / {}",
            ui.search_match.min(matches.len() - 1) + 1,
            matches.len()
        )
    };
    draw_text(
        ui,
        text_cache,
        packet,
        &counter,
        rect.x + rect.width - 78.0,
        rect.y + 9.0,
        11.0,
        66.0,
        MUTED,
    );
    if ui.pane_focused {
        let prefix = ui.search_query[..ui.search_cursor.min(ui.search_query.len())]
            .chars()
            .count() as f32;
        let cursor_x = (rect.x + 30.0 + prefix * 7.0).min(rect.x + rect.width - 92.0);
        packet.push(Command::FillRect {
            rect: Rect::new(cursor_x, rect.y + 8.0, 1.5, 17.0),
            color: BLUE,
        });
    }
}

fn draw_transcript(ui: &mut UiState, text_cache: &mut TextLayoutCache, packet: &mut Packet) {
    let lines = ui
        .selected_agent()
        .and_then(|agent| ui.views.get(agent))
        .map(|view| view.logs.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let viewport = transcript_rect(ui);
    ui.transcript_viewport_height = viewport.height;
    let query = if ui.search_active {
        ui.search_query.trim().to_lowercase()
    } else {
        String::new()
    };
    let content_width = (viewport.width - 28.0).max(120.0);
    let mut cards = Vec::new();
    for (log_index, line) in lines.iter().enumerate() {
        let blocks = prepare_markdown_blocks(ui, text_cache, line, content_width - 24.0);
        let height = if line.kind == "status" {
            30.0
        } else {
            (34.0 + blocks.iter().map(|block| block.height + 7.0).sum::<f32>()).max(46.0)
        };
        cards.push(PreparedCard {
            log_index,
            kind: line.kind.clone(),
            blocks,
            height,
            matched: !query.is_empty() && line.text.to_lowercase().contains(&query),
        });
    }
    let content_height = cards.iter().map(|card| card.height + 10.0).sum::<f32>() + 8.0;
    ui.transcript_content_height = content_height;
    let maximum = max_transcript_scroll(ui);
    if ui.transcript_follow_tail {
        ui.transcript_scroll = maximum;
    } else {
        ui.transcript_scroll = ui.transcript_scroll.min(maximum);
    }
    if ui.search_reveal_pending {
        let matches = cards.iter().filter(|card| card.matched).collect::<Vec<_>>();
        if let Some(target) = matches.get(ui.search_match.min(matches.len().saturating_sub(1))) {
            let mut target_y = 8.0;
            for card in &cards {
                if card.log_index == target.log_index {
                    break;
                }
                target_y += card.height + 10.0;
            }
            ui.transcript_scroll = (target_y - viewport.height * 0.3).clamp(0.0, maximum);
            ui.transcript_follow_tail = ui.transcript_scroll >= maximum - 1.0;
        }
        ui.search_reveal_pending = false;
    }

    packet.push(Command::Save);
    packet.push(Command::ClipRect(viewport));
    let mut content_y = 8.0;
    for card in &cards {
        let y = viewport.y + content_y - ui.transcript_scroll;
        if y + card.height >= viewport.y && y <= viewport.y + viewport.height {
            draw_transcript_card(
                ui,
                text_cache,
                packet,
                card,
                viewport.x + 4.0,
                y,
                content_width,
            );
        }
        content_y += card.height + 10.0;
    }
    if cards.is_empty() {
        draw_empty_transcript(ui, text_cache, packet, viewport);
    }
    packet.push(Command::Restore);
    draw_transcript_scrollbar(ui, packet);
}

fn prepare_markdown_blocks(
    ui: &mut UiState,
    text_cache: &mut TextLayoutCache,
    line: &LogLine,
    width: f32,
) -> Vec<PreparedBlock> {
    let source = if line.kind == "tool" || line.kind == "status" || line.kind == "error" {
        vec![MarkdownBlock {
            kind: MarkdownKind::Paragraph,
            text: line.text.clone(),
        }]
    } else {
        parse_markdown(&line.text)
    };
    source
        .into_iter()
        .map(|block| {
            let (size, color) = match block.kind {
                MarkdownKind::Heading => (16.0, BRIGHT),
                MarkdownKind::Code => (12.0, ashen::WARNING),
                MarkdownKind::Quote => (13.0, MUTED),
                MarkdownKind::Rule => (10.0, ashen::SUBTLE),
                _ => (
                    13.5,
                    if line.kind == "error" {
                        ashen::ERROR_BRIGHT
                    } else {
                        FG
                    },
                ),
            };
            let inset = if matches!(block.kind, MarkdownKind::Code | MarkdownKind::Quote) {
                18.0
            } else {
                0.0
            };
            let height = match text_cache.layout_text(
                &block.text,
                size,
                (width - inset).max(40.0),
                size * 1.45,
                color,
            ) {
                Ok(layout) => layout.height.max(size * 1.45),
                Err(error) => {
                    ui.draw_error
                        .get_or_insert_with(|| format!("layout transcript: {error}"));
                    size * 1.45
                }
            };
            PreparedBlock {
                kind: block.kind,
                text: block.text,
                size,
                color,
                height,
            }
        })
        .collect()
}

fn draw_transcript_card(
    ui: &mut UiState,
    text_cache: &mut TextLayoutCache,
    packet: &mut Packet,
    card: &PreparedCard,
    x: f32,
    y: f32,
    width: f32,
) {
    if card.kind == "status" {
        draw_text(
            ui,
            text_cache,
            packet,
            &format!(
                "·  {}",
                one_line(
                    card.blocks.first().map_or("", |block| block.text.as_str()),
                    160
                )
            ),
            x + 10.0,
            y + 6.0,
            11.5,
            width - 20.0,
            MUTED,
        );
        return;
    }
    let (background, accent, label) = match card.kind.as_str() {
        "user" => (PANEL_LIGHT, YELLOW, "YOU"),
        "tool" => (SUNKEN, BLUE, "TOOL CALL"),
        "error" => (Color::rgba(0.25, 0.07, 0.07, 0.64), RED, "ERROR"),
        _ => (PANEL, MUTED, "AGENT"),
    };
    let rect = Rect::new(x, y, width, card.height);
    packet.push(Command::FillRoundedRect {
        rect,
        radius: 7.0,
        color: background,
    });
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(x, y + 7.0, 3.0, card.height - 14.0),
        radius: 1.5,
        color: accent,
    });
    if card.matched {
        packet.push(Command::StrokeRoundedRect {
            rect,
            radius: 7.0,
            color: YELLOW,
            width: 1.5,
        });
    }
    draw_text(
        ui,
        text_cache,
        packet,
        label,
        x + 14.0,
        y + 9.0,
        10.0,
        width - 28.0,
        accent,
    );
    let mut block_y = y + 28.0;
    for block in &card.blocks {
        let inset = match block.kind {
            MarkdownKind::Bullet => 12.0,
            MarkdownKind::Quote | MarkdownKind::Code => 14.0,
            _ => 0.0,
        };
        if block.kind == MarkdownKind::Code {
            packet.push(Command::FillRoundedRect {
                rect: Rect::new(x + 12.0, block_y - 3.0, width - 24.0, block.height + 7.0),
                radius: 4.0,
                color: BG,
            });
        } else if block.kind == MarkdownKind::Quote {
            packet.push(Command::FillRect {
                rect: Rect::new(x + 14.0, block_y, 2.0, block.height),
                color: ACCENT_MUTED,
            });
        }
        draw_text(
            ui,
            text_cache,
            packet,
            &block.text,
            x + 14.0 + inset,
            block_y,
            block.size,
            width - 28.0 - inset,
            block.color,
        );
        block_y += block.height + 7.0;
    }
}

fn draw_empty_transcript(
    ui: &mut UiState,
    text_cache: &mut TextLayoutCache,
    packet: &mut Packet,
    viewport: Rect,
) {
    draw_text(
        ui,
        text_cache,
        packet,
        "Start a conversation",
        viewport.x + 18.0,
        viewport.y + viewport.height * 0.38,
        18.0,
        viewport.width - 36.0,
        STRONG,
    );
    draw_text(
        ui,
        text_cache,
        packet,
        "Type below, paste an image, or drop one into the pane.",
        viewport.x + 18.0,
        viewport.y + viewport.height * 0.38 + 30.0,
        13.0,
        viewport.width - 36.0,
        MUTED,
    );
}

fn draw_transcript_scrollbar(ui: &UiState, packet: &mut Packet) {
    let Some((_, _, thumb_y, thumb_height)) = scrollbar_geometry(ui) else {
        return;
    };
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(ui.width - 10.0, thumb_y, SCROLLBAR_WIDTH, thumb_height),
        radius: SCROLLBAR_WIDTH * 0.5,
        color: Color::rgba(
            FG.r,
            FG.g,
            FG.b,
            if ui.scrollbar_drag.is_some() {
                0.82
            } else if ui.scrollbar_hovered {
                0.62
            } else {
                0.35
            },
        ),
    });
}

fn parse_markdown(text: &str) -> Vec<MarkdownBlock> {
    let mut blocks = Vec::new();
    let mut paragraph = String::new();
    let mut code = String::new();
    let mut in_code = false;
    let flush_paragraph = |blocks: &mut Vec<MarkdownBlock>, paragraph: &mut String| {
        if !paragraph.trim().is_empty() {
            blocks.push(MarkdownBlock {
                kind: MarkdownKind::Paragraph,
                text: clean_inline_markdown(paragraph.trim()),
            });
            paragraph.clear();
        }
    };
    for line in text.replace(" ↵ ", "\n").lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if in_code {
                blocks.push(MarkdownBlock {
                    kind: MarkdownKind::Code,
                    text: std::mem::take(&mut code),
                });
            } else {
                flush_paragraph(&mut blocks, &mut paragraph);
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            if !code.is_empty() {
                code.push('\n');
            }
            code.push_str(line);
            continue;
        }
        if trimmed.is_empty() {
            flush_paragraph(&mut blocks, &mut paragraph);
        } else if trimmed == "---" || trimmed == "***" {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(MarkdownBlock {
                kind: MarkdownKind::Rule,
                text: "────────────────────────".to_string(),
            });
        } else if let Some(heading) = trimmed
            .strip_prefix("### ")
            .or_else(|| trimmed.strip_prefix("## "))
            .or_else(|| trimmed.strip_prefix("# "))
        {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(MarkdownBlock {
                kind: MarkdownKind::Heading,
                text: clean_inline_markdown(heading),
            });
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(MarkdownBlock {
                kind: MarkdownKind::Bullet,
                text: format!("•  {}", clean_inline_markdown(item)),
            });
        } else if let Some(quote) = trimmed.strip_prefix("> ") {
            flush_paragraph(&mut blocks, &mut paragraph);
            blocks.push(MarkdownBlock {
                kind: MarkdownKind::Quote,
                text: clean_inline_markdown(quote),
            });
        } else {
            if !paragraph.is_empty() {
                paragraph.push(' ');
            }
            paragraph.push_str(trimmed);
        }
    }
    flush_paragraph(&mut blocks, &mut paragraph);
    if in_code && !code.is_empty() {
        blocks.push(MarkdownBlock {
            kind: MarkdownKind::Code,
            text: code,
        });
    }
    if blocks.is_empty() {
        blocks.push(MarkdownBlock {
            kind: MarkdownKind::Paragraph,
            text: String::new(),
        });
    }
    blocks
}

fn clean_inline_markdown(text: &str) -> String {
    let mut output = text.replace("**", "").replace("__", "").replace('`', "");
    while let Some(open) = output.find('[') {
        let Some(middle) = output[open + 1..].find("](").map(|index| open + 1 + index) else {
            break;
        };
        let Some(close) = output[middle + 2..]
            .find(')')
            .map(|index| middle + 2 + index)
        else {
            break;
        };
        let label = output[open + 1..middle].to_string();
        output.replace_range(open..=close, &format!("{label} ↗"));
    }
    output
}

fn draw_prompt(ui: &mut UiState, text_cache: &mut TextLayoutCache, packet: &mut Packet) {
    let rect = prompt_rect(ui);
    let y = rect.y;
    if !ui.images.is_empty() {
        draw_image_tray(
            ui,
            text_cache,
            packet,
            y - IMAGE_TRAY_GAP - IMAGE_TRAY_HEIGHT,
        );
    }
    packet.push(Command::FillRoundedRect {
        rect,
        radius: 9.0,
        color: PANEL,
    });
    packet.push(Command::StrokeRoundedRect {
        rect,
        radius: 9.0,
        color: if ui.pane_focused && !ui.search_active {
            ACCENT_MUTED
        } else {
            ashen::SUBTLE
        },
        width: 1.0,
    });
    draw_text(
        ui,
        text_cache,
        packet,
        "MESSAGE",
        rect.x + 12.0,
        y + 8.0,
        9.5,
        80.0,
        MUTED,
    );
    let editor_width = (rect.width - 122.0).max(120.0);
    let columns = (editor_width / 7.4).max(16.0) as usize;
    let editor_lines = wrap_editor_text(&ui.prompt, columns);
    let cursor_line = editor_lines
        .iter()
        .position(|line| ui.prompt_cursor >= line.start && ui.prompt_cursor <= line.end)
        .unwrap_or_else(|| editor_lines.len().saturating_sub(1));
    let first_line = cursor_line.saturating_sub(1);
    for (row, line) in editor_lines.iter().skip(first_line).take(2).enumerate() {
        let text = if ui.prompt.is_empty() && row == 0 {
            "Ask or instruct the agent…"
        } else {
            &line.text
        };
        if ui.prompt_selection.is_some() && !ui.prompt.is_empty() {
            packet.push(Command::FillRoundedRect {
                rect: Rect::new(
                    rect.x + 11.0,
                    y + 25.0 + row as f32 * 20.0,
                    editor_width,
                    19.0,
                ),
                radius: 3.0,
                color: ashen::SELECTION,
            });
        }
        draw_text(
            ui,
            text_cache,
            packet,
            text,
            rect.x + 13.0,
            y + 27.0 + row as f32 * 20.0,
            14.0,
            editor_width,
            if ui.prompt.is_empty() { MUTED } else { FG },
        );
    }
    if ui.pane_focused
        && !ui.search_active
        && let Some(line) = editor_lines.get(cursor_line)
    {
        let cursor = ui.prompt_cursor.clamp(line.start, line.end);
        let column = ui.prompt[line.start..cursor].chars().count() as f32;
        let visible_row = cursor_line.saturating_sub(first_line).min(1) as f32;
        let cursor_x = (rect.x + 13.0 + column * 7.4).min(rect.x + 13.0 + editor_width);
        packet.push(Command::FillRect {
            rect: Rect::new(cursor_x, y + 26.0 + visible_row * 20.0, 2.0, 19.0),
            color: YELLOW,
        });
    }
    draw_text(
        ui,
        text_cache,
        packet,
        "Shift+Enter newline  ·  Ctrl+F search",
        rect.x + 13.0,
        y + rect.height - 17.0,
        9.5,
        (rect.width - 112.0).max(80.0),
        MUTED,
    );
    let send_rect = send_button_rect(ui);
    let send_pressed = ui.pressed.as_ref() == Some(&UiTarget::Send);
    let send_hovered = ui.hovered.as_ref() == Some(&UiTarget::Send);
    let send_enabled = can_submit(ui);
    packet.push(Command::FillRoundedRect {
        rect: send_rect,
        radius: 6.0,
        color: if !send_enabled {
            PANEL_LIGHT
        } else if send_pressed {
            PRESSED
        } else if send_hovered {
            ashen::ACCENT_GLOW
        } else {
            GREEN
        },
    });
    draw_text(
        ui,
        text_cache,
        packet,
        if send_enabled { "Send  ↵" } else { "Send" },
        send_rect.x + 10.0,
        send_rect.y + if send_pressed { 7.0 } else { 6.0 },
        11.5,
        54.0,
        if send_enabled { PANEL } else { MUTED },
    );
    if !ui.error.is_empty() {
        let error_y = if ui.images.is_empty() {
            y - 22.0
        } else {
            y - IMAGE_TRAY_GAP - IMAGE_TRAY_HEIGHT - 22.0
        };
        draw_text(
            ui,
            text_cache,
            packet,
            &one_line(&ui.error, 120),
            18.0,
            error_y.max(92.0),
            12.0,
            (ui.width - 36.0).max(100.0),
            RED,
        );
    }
}

#[derive(Clone, Debug)]
struct EditorLine {
    text: String,
    start: usize,
    end: usize,
}

fn wrap_editor_text(text: &str, columns: usize) -> Vec<EditorLine> {
    if text.is_empty() {
        return vec![EditorLine {
            text: String::new(),
            start: 0,
            end: 0,
        }];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    let mut current = String::new();
    let mut count = 0;
    for (index, character) in text.char_indices() {
        if character == '\n' || count >= columns {
            lines.push(EditorLine {
                text: std::mem::take(&mut current),
                start,
                end: index,
            });
            start = if character == '\n' {
                index + character.len_utf8()
            } else {
                index
            };
            count = 0;
            if character == '\n' {
                continue;
            }
        }
        current.push(character);
        count += 1;
    }
    lines.push(EditorLine {
        text: current,
        start,
        end: text.len(),
    });
    lines
}

fn draw_image_tray(
    ui: &mut UiState,
    text_cache: &mut TextLayoutCache,
    packet: &mut Packet,
    y: f32,
) {
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(12.0, y, ui.width - 24.0, IMAGE_TRAY_HEIGHT),
        radius: 8.0,
        color: PANEL,
    });
    let images = ui
        .images
        .iter()
        .map(|image| (image.id, image.name.clone()))
        .collect::<Vec<_>>();
    let count = images.len() as f32;
    let content_width = (ui.width - 48.0).max(1.0);
    let total_gaps = (count - 1.0).max(0.0) * 8.0;
    let card_width = ((content_width - total_gaps) / count).clamp(72.0, 150.0);
    for (index, (image_id, name)) in images.into_iter().enumerate() {
        let x = 18.0 + index as f32 * (card_width + 8.0);
        let card = Rect::new(x, y + 6.0, card_width, IMAGE_TRAY_HEIGHT - 12.0);
        packet.push(Command::FillRoundedRect {
            rect: card,
            radius: 6.0,
            color: PANEL_LIGHT,
        });
        packet.push(Command::StrokeRoundedRect {
            rect: card,
            radius: 6.0,
            color: HOVER,
            width: 1.0,
        });
        if let Some((handle, width, height)) = ui
            .image_previews
            .get(&image_id)
            .map(|preview| (preview.handle, preview.width, preview.height))
        {
            let image_width = (card.width - 12.0).max(1.0);
            let image_height = 76.0_f32.min(card.height - 28.0).max(1.0);
            let scale = (image_width / width as f32).min(image_height / height as f32);
            let destination_width = width as f32 * scale;
            let destination_height = height as f32 * scale;
            packet.push(Command::DrawImage {
                image: handle,
                destination: Rect::new(
                    card.x + (card.width - destination_width) * 0.5,
                    card.y + (image_height - destination_height) * 0.5,
                    destination_width,
                    destination_height,
                ),
            });
        }
        draw_text(
            ui,
            text_cache,
            packet,
            &one_line(&name, 24),
            card.x + 6.0,
            card.y + card.height - 18.0,
            11.0,
            (card.width - 12.0).max(1.0),
            MUTED,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    ui: &mut UiState,
    text_cache: &mut TextLayoutCache,
    packet: &mut Packet,
    text: &str,
    x: f32,
    y: f32,
    size: f32,
    width: f32,
    color: Color,
) {
    let width = width.max(1.0);
    let handle = match text_cache.layout_text(text, size, width, size * 1.35, color) {
        Ok(layout) => layout.handle,
        Err(error) => {
            ui.draw_error
                .get_or_insert_with(|| format!("layout text: {error}"));
            return;
        }
    };
    packet.push(Command::DrawTextLayout {
        layout: handle,
        x,
        y,
    });
}

fn one_line(text: &str, maximum: usize) -> String {
    let mut output = text
        .replace('\r', "")
        .replace('\n', " ↵ ")
        .chars()
        .take(maximum)
        .collect::<String>();
    if text.chars().count() > maximum {
        output.push('…');
    }
    output
}

fn bounded_text(text: &str, maximum: usize) -> String {
    let mut output = text.chars().take(maximum).collect::<String>();
    if text.chars().count() > maximum {
        output.push('…');
    }
    output
}

async fn run_brood_agent_turn(
    request: &TurnRequest,
    images: &[ImageAttachment],
) -> Result<Vec<u8>, String> {
    publish_worker_log(
        &request.agent,
        "status",
        &format!(
            "Starting Brood agent · model={} · reasoning={}",
            request.model, request.reasoning
        ),
    );
    let mut temporary_images = Vec::new();
    let mut runner_images = Vec::new();
    for image in images {
        let path = temporary_image_path(&request.agent, image);
        match host::worker_replace_file(path.clone(), image.bytes.clone()).await {
            Ok(written) if written == image.bytes.len() as u64 => {
                runner_images.push(RunnerImage {
                    path: path.clone(),
                    mime_type: image.mime_type.clone(),
                });
                temporary_images.push(path);
            }
            Ok(written) => {
                cleanup_temporary_images(&request.agent, &temporary_images).await;
                return Err(format!(
                    "write attached image {}: expected {} bytes, wrote {written}",
                    image.name,
                    image.bytes.len()
                ));
            }
            Err(error) => {
                cleanup_temporary_images(&request.agent, &temporary_images).await;
                return Err(format!("write attached image {}: {error:?}", image.name));
            }
        }
    }
    let runner_request = RunnerRequest {
        agent: &request.agent,
        prompt: &request.prompt,
        model: &request.model,
        reasoning: &request.reasoning,
        session_id: request.session_id,
        history: &request.history,
        images: runner_images,
    };
    let input = serde_json::to_vec(&runner_request)
        .map_err(|error| format!("encode Brood agent turn: {error}"))?;
    let process = host::subprocess_spawn("brood-agent", &[])
        .map_err(|error| format!("spawn Brood agent: {error:?}"))?;
    if let Err(error) = write_all(process.stdin, &input).await {
        cleanup_temporary_images(&request.agent, &temporary_images).await;
        return Err(format!("write Brood agent request: {error}"));
    }
    let _ = host::stream_close_write(process.stdin);
    let result = drain_brood_agent_process(&request.agent, &process).await;
    let _ = host::stream_drop(process.stdin).await;
    let _ = host::stream_drop(process.stdout).await;
    let _ = host::stream_drop(process.stderr).await;
    let _ = host::child_drop(process.child).await;
    cleanup_temporary_images(&request.agent, &temporary_images).await;
    result
}

fn temporary_image_path(agent: &str, image: &ImageAttachment) -> String {
    let safe_agent = agent
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .take(48)
        .collect::<String>();
    format!(
        ".brood-agent-image-{safe_agent}-{}.{}",
        image.id,
        image_extension(&image.mime_type)
    )
}

fn image_extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

async fn cleanup_temporary_images(agent: &str, paths: &[String]) {
    for path in paths {
        if let Err(error) = host::worker_remove_file(path.clone()).await {
            publish_worker_log(
                agent,
                "error",
                &format!("Could not remove temporary image {path}: {error:?}"),
            );
        }
    }
}

async fn drain_brood_agent_process(
    agent: &str,
    process: &host::ProcessIo,
) -> Result<Vec<u8>, String> {
    let stdout_notification =
        host::stream_notification(process.stdout).map_err(runtime_error("stdout notification"))?;
    let stderr_notification =
        host::stream_notification(process.stderr).map_err(runtime_error("stderr notification"))?;
    let child_notification =
        host::child_notification(process.child).map_err(runtime_error("child notification"))?;
    let mut stdout_generation = 0;
    let mut stderr_generation = 0;
    let mut child_generation = 0;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut worker_output = None;
    let mut stdout_eof = false;
    let mut stderr_eof = false;

    loop {
        stdout_eof |= drain_agent_stream_lines(
            agent,
            process.stdout,
            "agent",
            &mut stdout,
            &mut worker_output,
        )?;
        stderr_eof |= drain_agent_stream_lines(
            agent,
            process.stderr,
            "error",
            &mut stderr,
            &mut worker_output,
        )?;
        let status = host::child_status(process.child).map_err(runtime_error("child status"))?;
        if status.0 != host::ChildState::Running && stdout_eof && stderr_eof {
            if !stdout.is_empty() {
                publish_worker_log(agent, "agent", &String::from_utf8_lossy(&stdout));
            }
            if !stderr.is_empty() {
                publish_worker_log(agent, "error", &String::from_utf8_lossy(&stderr));
            }
            let code = status.1.unwrap_or(-1);
            if status.0 == host::ChildState::Exited && code == 0 {
                return worker_output.ok_or_else(|| {
                    "Brood agent exited successfully without returning conversation state"
                        .to_string()
                });
            }
            return Err(format!("Brood agent exited with status {code}"));
        }

        let stdout_pollable = host::notification_pollable(stdout_notification, stdout_generation)
            .map_err(runtime_error("stdout pollable"))?;
        let stderr_pollable = host::notification_pollable(stderr_notification, stderr_generation)
            .map_err(runtime_error("stderr pollable"))?;
        let child_pollable = host::notification_pollable(child_notification, child_generation)
            .map_err(runtime_error("child pollable"))?;
        let ready = host::worker_wait_any(
            vec![stdout_pollable, stderr_pollable, child_pollable],
            None,
            host::WaitMode::Fair,
        )
        .await;
        let _ = host::pollable_drop(stdout_pollable);
        let _ = host::pollable_drop(stderr_pollable);
        let _ = host::pollable_drop(child_pollable);
        match ready {
            Ok(ready) if ready.index == 0 => stdout_generation = ready.generation,
            Ok(ready) if ready.index == 1 => stderr_generation = ready.generation,
            Ok(ready) => child_generation = ready.generation,
            Err(host::WaitError::Cancelled) => return Err("turn cancelled".to_string()),
            Err(error) => return Err(format!("wait for Brood agent output: {error:?}")),
        }
    }
}

fn drain_agent_stream_lines(
    agent: &str,
    stream: u64,
    fallback_kind: &str,
    pending: &mut Vec<u8>,
    worker_output: &mut Option<Vec<u8>>,
) -> Result<bool, String> {
    loop {
        match host::stream_read(stream, 8192).map_err(runtime_error("read Brood agent stream"))? {
            host::StreamReadResult::Data(bytes) => {
                pending.extend(bytes);
                while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                    let line = pending.drain(..=newline).collect::<Vec<_>>();
                    let line = String::from_utf8_lossy(&line);
                    let (kind, text, history) = summarize_agent_line(line.trim(), fallback_kind);
                    if let Some(history) = history {
                        *worker_output = Some(history);
                    }
                    if !text.is_empty() {
                        publish_worker_log(agent, kind, &text);
                    }
                }
            }
            host::StreamReadResult::WouldBlock => return Ok(false),
            host::StreamReadResult::Eof => return Ok(true),
        }
    }
}

fn summarize_agent_line(
    line: &str,
    fallback_kind: &str,
) -> (&'static str, String, Option<Vec<u8>>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return (
            if fallback_kind == "error" {
                "error"
            } else {
                "agent"
            },
            bounded_text(line, 3000),
            None,
        );
    };
    let event_type = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    match event_type {
        "assistant" => ("agent", event_text(&value), None),
        "reasoning" => ("status", event_text(&value), None),
        "status" => ("status", event_text(&value), None),
        "plan" => ("status", plan_event_text(&value), None),
        "tool_start" => {
            let tool = display_tool_name(&value);
            let detail = event_text(&value);
            (
                "tool",
                if detail.is_empty() {
                    format!("{tool} in progress")
                } else {
                    format!("{tool} · {}", one_line(&detail, 240))
                },
                None,
            )
        }
        "tool_end" => {
            let tool = display_tool_name(&value);
            let text = event_text(&value);
            let failed = value
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            (
                if failed { "error" } else { "tool" },
                format!(
                    "{tool} {}{}",
                    if failed { "failed" } else { "completed" },
                    if text.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", one_line(&text, 1000))
                    }
                ),
                None,
            )
        }
        "result" => (
            "status",
            String::new(),
            value
                .get("history")
                .and_then(|history| serde_json::to_vec(history).ok()),
        ),
        _ => (
            if fallback_kind == "error" {
                "error"
            } else {
                "status"
            },
            bounded_text(line, 3000),
            None,
        ),
    }
}

fn event_text(value: &serde_json::Value) -> String {
    value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn plan_event_text(value: &serde_json::Value) -> String {
    let mut lines = Vec::new();
    if let Some(text) = value.get("text").and_then(serde_json::Value::as_str)
        && !text.is_empty()
    {
        lines.push(text.to_string());
    }
    if let Some(steps) = value.get("steps").and_then(serde_json::Value::as_array) {
        for step in steps.iter().take(12) {
            let status = step
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pending");
            let label = step
                .get("step")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let marker = match status {
                "completed" => "✓",
                "in_progress" => "→",
                _ => "○",
            };
            if !label.is_empty() {
                lines.push(format!("{marker} {label}"));
            }
        }
    }
    bounded_text(&lines.join("\n"), 3000)
}

fn display_tool_name(value: &serde_json::Value) -> &str {
    value
        .get("tool")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool")
        .strip_prefix("brood_computer_use__")
        .unwrap_or_else(|| {
            value
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool")
        })
}

fn publish_worker_log(agent: &str, kind: &str, text: &str) {
    let notification = {
        let Ok(mut state) = STATE.lock() else {
            return;
        };
        let RoleState::Agent(agent_state) = &mut *state else {
            return;
        };
        agent_state.published_generation = agent_state.published_generation.saturating_add(1);
        let generation = agent_state.published_generation;
        agent_state.queued.push_back((
            generation,
            WireMessage {
                kind: "agent-log".to_string(),
                agent: agent.to_string(),
                log_kind: kind.to_string(),
                text: bounded_text(text, 3000),
                ..WireMessage::default()
            },
        ));
        (agent_state.notification, generation)
    };
    if notification.0 != 0 {
        let _ = host::notification_signal(notification.0, notification.1);
    }
}

fn runtime_error<T: core::fmt::Debug>(context: &'static str) -> impl FnOnce(T) -> String {
    move |error| format!("{context}: {error:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_agent_bind_seeds_a_non_empty_ui_before_registry_reply() {
        let mut ui = UiState::new();
        apply_ui_message(
            &mut ui,
            WireMessage {
                kind: "bind".to_string(),
                backend_id: 4,
                session_id: 9,
                agent: "coding-agent-1".to_string(),
                ..WireMessage::default()
            },
        );

        assert_eq!(ui.backend_id, 4);
        assert_eq!(ui.session_id, 9);
        assert_eq!(ui.selected_agent(), Some("coding-agent-1"));
        assert_eq!(
            ui.views
                .get("coding-agent-1")
                .map(|view| view.status.as_str()),
            Some("connecting")
        );

        apply_ui_message(
            &mut ui,
            WireMessage {
                kind: "registry".to_string(),
                session_id: 9,
                agents: vec![WireAgent {
                    id: "coding-agent-1".to_string(),
                    status: "idle".to_string(),
                    model: "gpt-5".to_string(),
                    reasoning: "high".to_string(),
                }],
                ..WireMessage::default()
            },
        );

        assert_eq!(ui.selected_agent(), Some("coding-agent-1"));
        let view = ui.views.get("coding-agent-1").unwrap();
        assert_eq!(view.status, "idle");
        assert_eq!(view.model, "gpt-5");
        assert_eq!(view.reasoning, "high");
    }

    #[test]
    fn brood_agent_assistant_events_are_rendered_as_agent_output() {
        assert_eq!(
            summarize_agent_line(r#"{"type":"assistant","text":"Done"}"#, "agent"),
            ("agent", "Done".to_string(), None)
        );
    }

    #[test]
    fn brood_agent_computer_tool_events_use_short_names() {
        assert_eq!(
            summarize_agent_line(
                r#"{"type":"tool_start","tool":"brood_computer_use__computer_launch"}"#,
                "agent"
            ),
            ("tool", "computer_launch in progress".to_string(), None)
        );
        assert_eq!(
            summarize_agent_line(
                r#"{"type":"tool_start","tool":"exec_command","text":"$ cargo test"}"#,
                "agent"
            ),
            ("tool", "exec_command · $ cargo test".to_string(), None)
        );
        assert_eq!(
            summarize_agent_line(
                r#"{"type":"tool_end","tool":"brood_computer_use__computer_launch","text":"{\"pane\":3,\"visible\":true}","is_error":false}"#,
                "agent"
            ),
            (
                "tool",
                "computer_launch completed: {\"pane\":3,\"visible\":true}".to_string(),
                None
            )
        );
    }

    #[test]
    fn brood_agent_plan_events_render_progress_markers() {
        assert_eq!(
            summarize_agent_line(
                r#"{"type":"plan","text":"Implementation plan","steps":[{"step":"Inspect","status":"completed"},{"step":"Fix","status":"in_progress"},{"step":"Verify","status":"pending"}]}"#,
                "agent"
            ),
            (
                "status",
                "Implementation plan\n✓ Inspect\n→ Fix\n○ Verify".to_string(),
                None
            )
        );
    }

    #[test]
    fn brood_agent_result_preserves_conversation_history() {
        let (_, text, history) = summarize_agent_line(
            r#"{"type":"result","history":[{"user":"hello","assistant":"hi"}]}"#,
            "agent",
        );
        assert!(text.is_empty());
        let history = serde_json::from_slice::<Vec<HistoryTurn>>(&history.unwrap()).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].assistant, "hi");
    }

    #[test]
    fn editor_inserts_and_deletes_at_unicode_cursor_boundaries() {
        let mut ui = UiState::new();
        insert_ui_text(&mut ui, "alpha γ");
        move_editor_cursor(&mut ui, false, false);
        insert_ui_text(&mut ui, "beta ");
        assert_eq!(ui.prompt, "alpha beta γ");
        assert_eq!(ui.prompt_cursor, "alpha beta ".len());
        assert!(delete_before_cursor(&mut ui.prompt, &mut ui.prompt_cursor));
        assert_eq!(ui.prompt, "alpha betaγ");
    }

    #[test]
    fn prompt_history_recalls_turns_and_restores_the_unsent_draft() {
        let mut ui = UiState::new();
        record_prompt_history(&mut ui, "first prompt");
        record_prompt_history(&mut ui, "second\nprompt");
        ui.prompt = "unsent λ draft".to_string();
        ui.prompt_cursor = 0;

        assert!(navigate_prompt_history(&mut ui, false));
        assert_eq!(ui.prompt, "second\nprompt");
        assert!(navigate_prompt_history(&mut ui, false));
        assert_eq!(ui.prompt, "first prompt");
        assert!(navigate_prompt_history(&mut ui, true));
        assert_eq!(ui.prompt, "second\nprompt");
        assert!(navigate_prompt_history(&mut ui, true));
        assert_eq!(ui.prompt, "unsent λ draft");
        assert_eq!(ui.prompt_cursor, ui.prompt.len());
        assert_eq!(ui.prompt_history_cursor, None);
    }

    #[test]
    fn prompt_history_only_takes_over_at_multiline_boundaries() {
        let mut ui = UiState::new();
        record_prompt_history(&mut ui, "older");
        ui.prompt = "top\nbottom".to_string();
        ui.prompt_cursor = ui.prompt.len();

        handle_ui_key(&mut ui, "ArrowUp", true, 0);
        assert_eq!(ui.prompt, "top\nbottom");
        assert_eq!(ui.prompt_cursor, 3);
        assert_eq!(ui.prompt_history_cursor, None);

        handle_ui_key(&mut ui, "ArrowUp", true, 0);
        assert_eq!(ui.prompt, "older");
        handle_ui_key(&mut ui, "ArrowDown", true, 0);
        assert_eq!(ui.prompt, "top\nbottom");
    }

    #[test]
    fn prompt_history_is_bounded_and_deduplicates_adjacent_turns() {
        let mut ui = UiState::new();
        for index in 0..=MAX_PROMPT_HISTORY {
            record_prompt_history(&mut ui, &format!("prompt {index}"));
        }
        record_prompt_history(&mut ui, "prompt 100");
        assert_eq!(ui.prompt_history.len(), MAX_PROMPT_HISTORY);
        assert_eq!(
            ui.prompt_history.front().map(String::as_str),
            Some("prompt 1")
        );
        assert_eq!(
            ui.prompt_history.back().map(String::as_str),
            Some("prompt 100")
        );
    }

    #[test]
    fn pointer_click_places_editor_cursor_and_ctrl_f_opens_search() {
        let mut ui = UiState::new();
        ui.prompt = "alpha beta".to_string();
        ui.prompt_cursor = ui.prompt.len();
        let rect = prompt_rect(&ui);
        set_prompt_cursor_from_pointer(&mut ui, rect.x + 13.0, rect.y + 28.0);
        assert_eq!(ui.prompt_cursor, 0);
        handle_ui_key(&mut ui, "f", true, MOD_CTRL);
        assert!(ui.search_active);
    }

    #[test]
    fn markdown_parser_builds_semantic_blocks_without_raw_markers() {
        let blocks = parse_markdown(
            "# Result\n\n- **first** item\n\n> note\n\n```rust\nlet value = 4;\n```",
        );
        assert_eq!(blocks[0].kind, MarkdownKind::Heading);
        assert_eq!(blocks[0].text, "Result");
        assert_eq!(blocks[1].kind, MarkdownKind::Bullet);
        assert_eq!(blocks[1].text, "•  first item");
        assert_eq!(blocks[2].kind, MarkdownKind::Quote);
        assert_eq!(blocks[3].kind, MarkdownKind::Code);
        assert!(blocks[3].text.contains("let value = 4;"));
    }

    #[test]
    fn transcript_scroll_disables_and_restores_tail_following() {
        let mut ui = UiState::new();
        ui.transcript_content_height = 900.0;
        ui.transcript_viewport_height = 300.0;
        ui.transcript_scroll = 600.0;
        assert!(scroll_transcript(&mut ui, -120.0));
        assert!(!ui.transcript_follow_tail);
        assert!(scroll_transcript(&mut ui, 500.0));
        assert_eq!(ui.transcript_scroll, 600.0);
        assert!(ui.transcript_follow_tail);
    }

    #[test]
    fn dragging_tabs_preserves_selected_agent_identity() {
        let mut ui = UiState::new();
        ui.agents = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        ui.selected = 1;
        ui.tab_drag = Some(TabDrag {
            agent: "one".to_string(),
            start_x: 20.0,
            moved: false,
        });
        assert!(update_tab_drag(&mut ui, 320.0));
        assert_eq!(ui.agents, ["two", "three", "one"]);
        assert_eq!(ui.selected_agent(), Some("two"));
    }

    fn png_bytes(size: usize) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(size.max(bytes.len()), 0x5a);
        bytes
    }

    #[test]
    fn image_chunks_stay_below_the_remote_actor_message_limit() {
        let image = ImageAttachment {
            id: 7,
            name: "proof.png".to_string(),
            mime_type: "image/png".to_string(),
            bytes: png_bytes(IMAGE_CHUNK_BYTES * 2 + 19),
        };
        let messages = image_transfer_messages(&image);
        assert_eq!(messages.len(), 5);
        for message in messages {
            assert!(serde_json::to_vec(&message).unwrap().len() < 64 * 1024);
        }
    }

    #[test]
    fn agent_reassembles_ordered_image_chunks() {
        let bytes = png_bytes(IMAGE_CHUNK_BYTES + 11);
        let mut agent = AgentState::new(1);
        begin_agent_image(
            &mut agent,
            &WireMessage {
                image_id: 9,
                image_name: "remote.png".to_string(),
                mime_type: "image/png".to_string(),
                total_bytes: bytes.len() as u64,
                ..WireMessage::default()
            },
        )
        .unwrap();
        for (index, chunk) in bytes.chunks(IMAGE_CHUNK_BYTES).enumerate() {
            append_agent_image(
                &mut agent,
                &WireMessage {
                    image_id: 9,
                    offset: (index * IMAGE_CHUNK_BYTES) as u64,
                    data: hex_encode(chunk),
                    ..WireMessage::default()
                },
            )
            .unwrap();
        }
        finish_agent_image(&mut agent, 9).unwrap();
        assert_eq!(agent.draft_images.get(&9).unwrap().bytes, bytes);
    }

    #[test]
    fn agent_rejects_out_of_order_image_chunks() {
        let mut agent = AgentState::new(1);
        begin_agent_image(
            &mut agent,
            &WireMessage {
                image_id: 2,
                image_name: "bad.png".to_string(),
                mime_type: "image/png".to_string(),
                total_bytes: 20,
                ..WireMessage::default()
            },
        )
        .unwrap();
        let error = append_agent_image(
            &mut agent,
            &WireMessage {
                image_id: 2,
                offset: 4,
                data: "89504e47".to_string(),
                ..WireMessage::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("out of order"));
    }
}

bindings::export!(CodingAgentPlugin with_types_in bindings);
