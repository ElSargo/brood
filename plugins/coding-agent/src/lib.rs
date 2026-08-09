use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
    graphics::{drop_resources, layout_ui_text, submit_packet, submit_tagged_packet},
    input::BUTTON_PRIMARY as LEFT_BUTTON,
};
use serde::{Deserialize, Serialize};

const COMPONENT_FILE: &str = "brood_coding_agent_plugin.component.wasm";
const CLIENT_INIT_ID: &str = "brood.client-init";
const UI_ID: &str = "coding-agent-ui";
const INBOX_ID: &str = "inbox";
const FIRST_AGENT: &str = "coding-agent-1";
const WORKER_ID: u64 = 1;
const UPDATE_NOTIFICATION_TOKEN: u64 = 0x434f_4445_5841_4745;
const MAX_LOG_LINES: usize = 96;
const MAX_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 12 * 1024 * 1024;
const IMAGE_CHUNK_BYTES: usize = 20 * 1024;
const PROMPT_HEIGHT: f32 = 60.0;
const IMAGE_TRAY_HEIGHT: f32 = 112.0;
const IMAGE_TRAY_GAP: f32 = 8.0;
const MOD_CTRL: u32 = 1;
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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TextLayoutKey {
    text: String,
    size: u32,
    width: u32,
    color: [u32; 4],
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
}

impl AgentState {
    fn new() -> Self {
        Self {
            id: String::new(),
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
    Model,
    Reasoning,
    Send,
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
    images: Vec<ImageAttachment>,
    image_previews: BTreeMap<u64, ImagePreview>,
    retired_image_previews: Vec<u64>,
    next_image: u64,
    suppressed_text: Option<String>,
    paste_key_pressed: bool,
    text_resources: BTreeMap<TextLayoutKey, u64>,
    frame_text_resources: BTreeSet<TextLayoutKey>,
    created_text_resources: Vec<TextLayoutKey>,
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
            images: Vec::new(),
            image_previews: BTreeMap::new(),
            retired_image_previews: Vec::new(),
            next_image: 1,
            suppressed_text: None,
            paste_key_pressed: false,
            text_resources: BTreeMap::new(),
            frame_text_resources: BTreeSet::new(),
            created_text_resources: Vec::new(),
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
    Agent(AgentState),
    Ui(UiState),
}

static STATE: Mutex<RoleState> = Mutex::new(RoleState::Unknown);

struct CodingAgentPlugin;

impl Guest for CodingAgentPlugin {
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
        run_codex_turn(&request, &images).await
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
                *state = RoleState::Ui(ui);
            }
            vec![ActorCommand::LifecycleComplete(Ok(()))]
        }
        LifecycleEvent::Start(context) if context.instance_id.contains(".coding-agent-") => {
            if let Ok(mut state) = STATE.lock() {
                *state = RoleState::Agent(AgentState::new());
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
    start_session_actor(
        id,
        COMPONENT_FILE,
        Vec::new(),
        backend_id,
        session_id,
    )
}

fn start_agent_command(id: &str) -> ActorCommand {
    start_global_actor(
        id,
        COMPONENT_FILE,
        vec![
            "filesystem:workspace-write".to_string(),
            "subprocess:codex".to_string(),
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
            let mut agent = AgentState::new();
            let commands = handle_agent_message(&mut agent, sender, message);
            *state = RoleState::Agent(agent);
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
        *state = RoleState::Ui(UiState::new());
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
            redraw_ui_for_resize(ui, resized.resize_epoch);
            return Vec::new();
        }
        PresentationEvent::PaneFocused(focused) if focused.pane == ui.pane && !focused.focused => {
            ui.hovered = None;
            ui.pressed = None;
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
            );
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
            let mut commands = Vec::new();
            for character in text.text.chars() {
                if matches!(character, '\r' | '\n') {
                    commands.extend(submit_prompt(ui));
                } else if !character.is_control() {
                    ui.prompt.push(character);
                }
            }
            (commands, true)
        }
        PresentationEvent::Paste(paste) if paste.pane == ui.pane => {
            ui.prompt.extend(
                paste.text.chars().filter(|character| {
                    !matches!(character, '\r' | '\n') && !character.is_control()
                }),
            );
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
            let changed = ui.hovered != hovered;
            ui.hovered = hovered;
            (Vec::new(), changed)
        }
        PresentationEvent::PointerLeft(left) if left.pane == ui.pane => {
            let changed = ui.hovered.take().is_some() || ui.pressed.take().is_some();
            (Vec::new(), changed)
        }
        PresentationEvent::PointerButton(button)
            if button.pane == ui.pane && button.button == LEFT_BUTTON =>
        {
            let target = ui_target_at(ui, button.x, button.y);
            if button.pressed {
                ui.hovered.clone_from(&target);
                ui.pressed = target;
                (Vec::new(), true)
            } else {
                let pressed = ui.pressed.take();
                ui.hovered.clone_from(&target);
                let commands = match (pressed, target) {
                    (Some(pressed), Some(released)) if pressed == released => {
                        activate_ui_target(ui, pressed)
                    }
                    _ => Vec::new(),
                };
                (commands, true)
            }
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
            ui.preferred_agent = (!message.agent.is_empty()).then_some(message.agent);
        }
        "registry" => {
            if message.session_id != 0 && message.session_id != ui.session_id {
                return;
            }
            let selected = ui
                .preferred_agent
                .take()
                .or_else(|| ui.selected_agent().map(str::to_string));
            ui.agents = message
                .agents
                .iter()
                .map(|agent| agent.id.clone())
                .collect();
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
            }
        }
        "agent-log" => {
            if let Some(view) = ui.views.get_mut(&message.agent) {
                push_log(view, message.log_kind, message.text);
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
    if pressed {
        if key == "Backspace" {
            if ui.prompt.is_empty() {
                if let Some(image) = ui.images.pop() {
                    retire_image_preview(ui, image.id);
                }
            } else {
                ui.prompt.pop();
            }
        }
        return Vec::new();
    }
    match key {
        "Enter" | "Return" | "NumpadEnter" => submit_prompt(ui),
        "Tab" => {
            if !ui.agents.is_empty() {
                ui.selected = (ui.selected + 1) % ui.agents.len();
            }
            Vec::new()
        }
        "F2" => create_agent(ui).into_iter().collect(),
        "F6" => configure_selected(ui, true).into_iter().collect(),
        "F7" => configure_selected(ui, false).into_iter().collect(),
        "Escape" => cancel_selected(ui).into_iter().collect(),
        _ => Vec::new(),
    }
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
    None
}

fn activate_ui_target(ui: &mut UiState, target: UiTarget) -> Vec<ActorCommand> {
    match target {
        UiTarget::AgentTab(index) if index < ui.agents.len() => {
            ui.selected = index;
            Vec::new()
        }
        UiTarget::Model => configure_selected(ui, true).into_iter().collect(),
        UiTarget::Reasoning => configure_selected(ui, false).into_iter().collect(),
        UiTarget::Send => submit_prompt(ui),
        UiTarget::AgentTab(_) => Vec::new(),
    }
}

fn agent_tab_width(ui: &UiState) -> f32 {
    if ui.agents.is_empty() {
        152.0
    } else {
        ((ui.width - 24.0) / ui.agents.len() as f32).clamp(48.0, 152.0)
    }
}

fn agent_tab_rect(ui: &UiState, index: usize) -> Rect {
    let width = agent_tab_width(ui);
    Rect::new(12.0 + index as f32 * (width + 4.0), 8.0, width, 28.0)
}

fn toolbar_control_rects(width: f32) -> (Rect, Rect) {
    let content_width = (width - 32.0).max(1.0);
    let status_width = (content_width * 0.25).clamp(110.0, 190.0);
    let control_width = ((content_width - status_width - 16.0) * 0.5).clamp(96.0, 250.0);
    let model = Rect::new(16.0 + status_width + 8.0, 52.0, control_width, 32.0);
    let reasoning = Rect::new(model.x + model.width + 8.0, 52.0, control_width, 32.0);
    (model, reasoning)
}

fn send_button_rect(ui: &UiState) -> Rect {
    Rect::new(ui.width - 104.0, ui.height - 68.0, 80.0, 38.0)
}

fn rect_contains(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
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
    let prompt = if ui.prompt.trim().is_empty() {
        "Describe the attached image(s).".to_string()
    } else {
        std::mem::take(&mut ui.prompt)
    };
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
    ui.frame_text_resources.clear();
    ui.created_text_resources.clear();
    ui.draw_error = None;
    let mut packet = Packet::new();
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, ui.width, ui.height),
        color: BG,
    });
    draw_agent_tabs(ui, &mut packet);
    draw_toolbar(ui, &mut packet);
    draw_transcript(ui, &mut packet);
    draw_prompt(ui, &mut packet);
    let submitted = ui.draw_error.take().map_or_else(
        || {
            resize_epoch.map_or_else(
                || submit_packet(ui.pane, &packet).map(|_| ()),
                |resize_epoch| submit_tagged_packet(ui.pane, resize_epoch, &packet).map(|_| ()),
            )
        },
        Err,
    );
    match submitted {
        Ok(()) => {
            drop_resources(ui.pane, ui.retired_image_previews.drain(..));
            let stale = ui
                .text_resources
                .keys()
                .filter(|key| !ui.frame_text_resources.contains(*key))
                .cloned()
                .collect::<Vec<_>>();
            for key in stale {
                if let Some(resource) = ui.text_resources.remove(&key) {
                    drop_resources(ui.pane, [resource]);
                }
            }
            ui.created_text_resources.clear();
            ui.error.clear();
        }
        Err(error) => {
            for key in std::mem::take(&mut ui.created_text_resources) {
                if let Some(resource) = ui.text_resources.remove(&key) {
                    drop_resources(ui.pane, [resource]);
                }
            }
            ui.error = error;
        }
    }
    ui.frame_text_resources.clear();
}

fn draw_agent_tabs(ui: &mut UiState, packet: &mut Packet) {
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, ui.width, 44.0),
        color: PANEL,
    });
    let agents = ui.agents.clone();
    for (index, agent) in agents.iter().enumerate() {
        let rect = agent_tab_rect(ui, index);
        let target = UiTarget::AgentTab(index);
        let pressed = ui.pressed.as_ref() == Some(&target);
        let hovered = ui.hovered.as_ref() == Some(&target);
        packet.push(Command::FillRoundedRect {
            rect,
            radius: 5.0,
            color: if pressed {
                PRESSED
            } else if hovered {
                HOVER
            } else if index == ui.selected {
                BLUE
            } else {
                PANEL_LIGHT
            },
        });
        draw_text(
            ui,
            packet,
            agent,
            rect.x + 7.0,
            if pressed { 13.0 } else { 12.0 },
            14.0,
            (rect.width - 14.0).max(24.0),
            FG,
        );
    }
}

fn draw_toolbar(ui: &mut UiState, packet: &mut Packet) {
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
        rect: Rect::new(0.0, 44.0, ui.width, 48.0),
        color: PANEL_LIGHT,
    });
    let (model_rect, reasoning_rect) = toolbar_control_rects(ui.width);
    draw_text(
        ui,
        packet,
        &format!("status  {status}"),
        16.0,
        59.0,
        14.0,
        (model_rect.x - 32.0).max(80.0),
        if status == "running" { GREEN } else { FG },
    );
    draw_toolbar_button(
        ui,
        packet,
        UiTarget::Model,
        model_rect,
        &format!("Model  {model}"),
        BLUE,
    );
    draw_toolbar_button(
        ui,
        packet,
        UiTarget::Reasoning,
        reasoning_rect,
        &format!("Reasoning  {reasoning}"),
        YELLOW,
    );
}

fn draw_toolbar_button(
    ui: &mut UiState,
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
        radius: 5.0,
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
        packet,
        label,
        rect.x + 9.0,
        rect.y + if pressed { 8.0 } else { 7.0 },
        13.0,
        rect.width - 18.0,
        if ui.selected_agent().is_some() {
            label_color
        } else {
            MUTED
        },
    );
}

fn draw_transcript(ui: &mut UiState, packet: &mut Packet) {
    let lines = ui
        .selected_agent()
        .and_then(|agent| ui.views.get(agent))
        .map(|view| view.logs.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let composer_extra = if ui.images.is_empty() {
        0.0
    } else {
        IMAGE_TRAY_HEIGHT + IMAGE_TRAY_GAP
    };
    let available = ((ui.height - 190.0 - composer_extra) / 24.0).max(1.0) as usize;
    let start = lines.len().saturating_sub(available);
    let mut y = 108.0;
    for line in lines.iter().skip(start) {
        let color = match line.kind.as_str() {
            "user" => YELLOW,
            "tool" => BLUE,
            "error" => RED,
            "status" => MUTED,
            _ => FG,
        };
        let prefix = match line.kind.as_str() {
            "user" => "you  ",
            "tool" => "tool ",
            "error" => "err  ",
            "status" => "·    ",
            _ => "agent ",
        };
        let visible_characters = ((ui.width - 36.0) / 8.0).max(24.0) as usize;
        let body_characters = visible_characters
            .saturating_sub(prefix.chars().count())
            .clamp(16, 180);
        let text = format!("{prefix}{}", one_line(&line.text, body_characters));
        draw_text(
            ui,
            packet,
            &text,
            18.0,
            y,
            14.0,
            (ui.width - 36.0).max(100.0),
            color,
        );
        y += 24.0;
    }
}

fn draw_prompt(ui: &mut UiState, packet: &mut Packet) {
    let y = ui.height - PROMPT_HEIGHT - 18.0;
    if !ui.images.is_empty() {
        draw_image_tray(ui, packet, y - IMAGE_TRAY_GAP - IMAGE_TRAY_HEIGHT);
    }
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(12.0, y, ui.width - 24.0, PROMPT_HEIGHT),
        radius: 8.0,
        color: PANEL,
    });
    draw_text(
        ui,
        packet,
        &format!("> {}", one_line(&ui.prompt, 150)),
        24.0,
        y + 12.0,
        15.0,
        (ui.width - 150.0).max(100.0),
        FG,
    );
    let send_rect = send_button_rect(ui);
    let send_pressed = ui.pressed.as_ref() == Some(&UiTarget::Send);
    let send_hovered = ui.hovered.as_ref() == Some(&UiTarget::Send);
    let send_enabled = can_submit(ui);
    packet.push(Command::FillRoundedRect {
        rect: send_rect,
        radius: 7.0,
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
        packet,
        if send_enabled { "Send" } else { "Enter" },
        ui.width - 88.0,
        send_rect.y + if send_pressed { 11.0 } else { 10.0 },
        14.0,
        56.0,
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

fn draw_image_tray(ui: &mut UiState, packet: &mut Packet, y: f32) {
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
    packet: &mut Packet,
    text: &str,
    x: f32,
    y: f32,
    size: f32,
    width: f32,
    color: Color,
) {
    let width = width.max(1.0);
    let key = TextLayoutKey {
        text: text.to_string(),
        size: size.to_bits(),
        width: width.to_bits(),
        color: [
            color.r.to_bits(),
            color.g.to_bits(),
            color.b.to_bits(),
            color.a.to_bits(),
        ],
    };
    ui.frame_text_resources.insert(key.clone());
    let handle = if let Some(handle) = ui.text_resources.get(&key) {
        *handle
    } else {
        let layout = match layout_ui_text(ui.pane, text, size, width, size * 1.35, color) {
            Ok(layout) => layout,
            Err(error) => {
                ui.draw_error
                    .get_or_insert_with(|| format!("layout text: {error}"));
                return;
            }
        };
        ui.text_resources.insert(key.clone(), layout.handle);
        ui.created_text_resources.push(key);
        layout.handle
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

async fn run_codex_turn(
    request: &TurnRequest,
    images: &[ImageAttachment],
) -> Result<Vec<u8>, String> {
    publish_worker_log(
        &request.agent,
        "status",
        &format!(
            "Starting Codex · model={} · reasoning={}",
            request.model, request.reasoning
        ),
    );
    let mut arguments = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--ephemeral".to_string(),
        "--skip-git-repo-check".to_string(),
        "--color".to_string(),
        "never".to_string(),
        "--sandbox".to_string(),
        "workspace-write".to_string(),
        "--config".to_string(),
        format!("model_reasoning_effort=\"{}\"", request.reasoning),
    ];
    if request.model != "default" {
        arguments.push("--model".to_string());
        arguments.push(request.model.clone());
    }
    let mut temporary_images = Vec::new();
    for image in images {
        let path = temporary_image_path(&request.agent, image);
        match host::worker_replace_file(path.clone(), image.bytes.clone()).await {
            Ok(written) if written == image.bytes.len() as u64 => {
                arguments.push("--image".to_string());
                arguments.push(path.clone());
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
    // `--image` accepts one or more values, so terminate option parsing before
    // the positional prompt or clap will consume it as another image path.
    arguments.push("--".to_string());
    arguments.push(request.prompt.clone());
    let process = match host::subprocess_spawn("codex", &arguments) {
        Ok(process) => process,
        Err(direct_error) => {
            let mut shell_arguments = vec![
                "-lic".to_string(),
                "exec codex \"$@\"".to_string(),
                "brood-codex".to_string(),
            ];
            shell_arguments.extend(arguments);
            match host::subprocess_spawn("bash", &shell_arguments) {
                Ok(process) => process,
                Err(shell_error) => {
                    cleanup_temporary_images(&request.agent, &temporary_images).await;
                    return Err(format!(
                        "spawn Codex directly ({direct_error:?}) or through a login shell ({shell_error:?})"
                    ));
                }
            }
        }
    };
    let _ = host::stream_close_write(process.stdin);
    let result = drain_codex_process(&request.agent, &process).await;
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

async fn drain_codex_process(agent: &str, process: &host::ProcessIo) -> Result<Vec<u8>, String> {
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
    let mut stdout_eof = false;
    let mut stderr_eof = false;

    loop {
        stdout_eof |= drain_stream_lines(agent, process.stdout, "agent", &mut stdout)?;
        stderr_eof |= drain_stream_lines(agent, process.stderr, "error", &mut stderr)?;
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
                publish_worker_log(agent, "status", "Turn complete");
                return Ok(Vec::new());
            }
            return Err(format!("codex exited with status {code}"));
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
            Err(error) => return Err(format!("wait for codex output: {error:?}")),
        }
    }
}

fn drain_stream_lines(
    agent: &str,
    stream: u64,
    fallback_kind: &str,
    pending: &mut Vec<u8>,
) -> Result<bool, String> {
    loop {
        match host::stream_read(stream, 8192).map_err(runtime_error("read codex stream"))? {
            host::StreamReadResult::Data(bytes) => {
                pending.extend(bytes);
                while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                    let line = pending.drain(..=newline).collect::<Vec<_>>();
                    let line = String::from_utf8_lossy(&line);
                    let (kind, text) = summarize_codex_line(line.trim(), fallback_kind);
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

fn summarize_codex_line(line: &str, fallback_kind: &str) -> (&'static str, String) {
    if line == "Reading additional input from stdin..." {
        return ("status", String::new());
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return (
            if fallback_kind == "error" {
                "error"
            } else {
                "agent"
            },
            one_line(line, 3000),
        );
    };
    let event_type = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    match event_type {
        "thread.started" => (
            "status",
            format!(
                "Thread {}",
                value
                    .get("thread_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("started")
            ),
        ),
        "turn.started" => ("status", "Thinking…".to_string()),
        "turn.completed" => ("status", "Model turn completed".to_string()),
        "error" => (
            "error",
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(line)
                .to_string(),
        ),
        "item.completed" | "item.started" | "item.updated" => {
            let item = value.get("item").unwrap_or(&value);
            let item_type = item
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            match item_type {
                "agent_message" => (
                    "agent",
                    item.get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
                "command_execution" => (
                    "tool",
                    format!(
                        "$ {} {}",
                        item.get("command")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("command"),
                        item.get("status")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                    ),
                ),
                "file_change" => ("tool", "Applied file changes".to_string()),
                _ => ("status", one_line(item_type, 3000)),
            }
        }
        _ => (
            if fallback_kind == "error" {
                "error"
            } else {
                "status"
            },
            one_line(line, 3000),
        ),
    }
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
                text: one_line(text, 3000),
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
        let mut agent = AgentState::new();
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
        let mut agent = AgentState::new();
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
