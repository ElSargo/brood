wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Mutex,
};

use brood::plugin::host;
use brood_graphics::{Color, Command, Packet, Rect};
use exports::brood::plugin::actor::{
    ActorStartRequest, ChildEvent, Command as ActorCommand, Event, Guest, LifecycleEvent,
    LocalActorMessage, PresentationActorMessage, PresentationEvent, SessionActorMessage,
    WorkerEvent, WorkerRequest,
};
use serde::{Deserialize, Serialize};

const COMPONENT_FILE: &str = "brood_coding_agent_plugin.component.wasm";
const CLIENT_INIT_ID: &str = "brood.client-init";
const UI_ID: &str = "coding-agent-ui";
const FIRST_AGENT: &str = "coding-agent-1";
const WORKER_ID: u64 = 1;
const UPDATE_NOTIFICATION_TOKEN: u64 = 0x434f_4445_5841_4745;
const LEFT_BUTTON: u32 = 0x110;
const MAX_LOG_LINES: usize = 96;

const BG: Color = Color::rgba(0.157, 0.157, 0.157, 1.0);
const PANEL: Color = Color::rgba(0.118, 0.118, 0.118, 1.0);
const PANEL_LIGHT: Color = Color::rgba(0.235, 0.220, 0.204, 1.0);
const FG: Color = Color::rgba(0.922, 0.859, 0.698, 1.0);
const MUTED: Color = Color::rgba(0.659, 0.600, 0.518, 1.0);
const YELLOW: Color = Color::rgba(0.843, 0.600, 0.129, 1.0);
const BLUE: Color = Color::rgba(0.271, 0.522, 0.533, 1.0);
const GREEN: Color = Color::rgba(0.596, 0.592, 0.102, 1.0);
const RED: Color = Color::rgba(0.800, 0.141, 0.114, 1.0);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct WireMessage {
    kind: String,
    action: String,
    session_id: u64,
    agent: String,
    prompt: String,
    model: String,
    reasoning: String,
    status: String,
    log_kind: String,
    text: String,
    agents: Vec<WireAgent>,
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
    notification: u64,
    published_generation: u64,
    observed_generation: u64,
    queued: VecDeque<(u64, WireMessage)>,
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
            notification: 0,
            published_generation: 0,
            observed_generation: 0,
            queued: VecDeque::new(),
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

struct UiState {
    pane: u64,
    width: f32,
    height: f32,
    session_id: u64,
    agents: Vec<String>,
    views: BTreeMap<String, AgentView>,
    selected: usize,
    preferred_agent: Option<String>,
    prompt: String,
    text_resources: BTreeMap<TextLayoutKey, u64>,
    frame_text_resources: BTreeSet<TextLayoutKey>,
    created_text_resources: Vec<TextLayoutKey>,
    draw_error: Option<String>,
    updates_since_draw: usize,
    error: String,
}

impl UiState {
    fn new() -> Self {
        Self {
            pane: 0,
            width: 900.0,
            height: 700.0,
            session_id: 0,
            agents: Vec::new(),
            views: BTreeMap::new(),
            selected: 0,
            preferred_agent: None,
            prompt: String::new(),
            text_resources: BTreeMap::new(),
            frame_text_resources: BTreeSet::new(),
            created_text_resources: Vec::new(),
            draw_error: None,
            updates_since_draw: 0,
            error: String::new(),
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
                handle_agent_notification(event.notification, event.generation, event.token)
            }
            Event::Worker(event) => handle_worker_event(event),
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        let request: TurnRequest =
            serde_json::from_slice(&payload).map_err(|error| format!("decode turn: {error}"))?;
        run_codex_turn(&request).await
    }
}

fn handle_lifecycle(event: LifecycleEvent) -> Vec<ActorCommand> {
    match event {
        LifecycleEvent::Start(context) if context.instance_id == CLIENT_INIT_ID => {
            let session_id = context.session_id.unwrap_or_default();
            set_root_state(true, session_id);
            vec![
                start_ui_command(UI_ID, session_id),
                ActorCommand::LifecycleComplete(Ok(())),
            ]
        }
        LifecycleEvent::Start(context) if context.instance_id.starts_with("coding-agent-ui") => {
            let mut ui = UiState::new();
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
            set_root_state(false, session_id);
            vec![
                start_agent_command(FIRST_AGENT),
                ActorCommand::LifecycleComplete(Ok(())),
            ]
        }
        LifecycleEvent::Stop(_) => vec![ActorCommand::LifecycleComplete(Ok(()))],
    }
}

fn set_root_state(client: bool, session_id: u64) {
    let Ok(mut state) = STATE.lock() else {
        return;
    };
    *state = RoleState::Root(RootState {
        client,
        session_id,
        agents: Vec::new(),
        subscribers: Vec::new(),
        next_agent: 2,
        next_ui: 2,
    });
}

fn start_ui_command(id: &str, session_id: u64) -> ActorCommand {
    ActorCommand::StartActor(ActorStartRequest {
        id: id.to_string(),
        path: COMPONENT_FILE.to_string(),
        capabilities: Vec::new(),
        session_id: Some(session_id),
    })
}

fn start_agent_command(id: &str) -> ActorCommand {
    ActorCommand::StartActor(ActorStartRequest {
        id: id.to_string(),
        path: COMPONENT_FILE.to_string(),
        capabilities: vec![
            "filesystem:workspace-write".to_string(),
            "subprocess:codex".to_string(),
        ],
        session_id: None,
    })
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
                vec![start_ui_command(&id, root.session_id)]
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
                agent.subscribers.push(subscriber);
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
        "prompt" if agent.active_worker.is_none() => {
            agent.id = message.agent;
            agent.model = message.model;
            agent.reasoning = message.reasoning;
            agent.status = "running".to_string();
            agent.active_worker = Some(WORKER_ID);
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
                    text: request.prompt.clone(),
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

fn handle_agent_notification(notification: u64, _generation: u64, token: u64) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let RoleState::Agent(agent) = &mut *state else {
        return Vec::new();
    };
    if notification != agent.notification || token != UPDATE_NOTIFICATION_TOKEN {
        return Vec::new();
    }
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
            agent.status = "idle".to_string();
            agent_commands(
                agent,
                &WireMessage {
                    kind: "agent-state".to_string(),
                    agent: agent.id.clone(),
                    status: agent.status.clone(),
                    ..WireMessage::default()
                },
            )
        }
        WorkerEvent::Failed(failed) if failed.worker == WORKER_ID => {
            agent.active_worker = None;
            agent.status = "failed".to_string();
            agent_commands(
                agent,
                &WireMessage {
                    kind: "agent-state".to_string(),
                    agent: agent.id.clone(),
                    status: agent.status.clone(),
                    log_kind: "error".to_string(),
                    text: failed.message,
                    ..WireMessage::default()
                },
            )
        }
        WorkerEvent::Cancelled(worker) if worker == WORKER_ID => {
            agent.active_worker = None;
            agent.status = "idle".to_string();
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
    serde_json::to_vec(message).ok().map(|body| {
        ActorCommand::SendLocalMessage(LocalActorMessage {
            actor: actor.to_string(),
            body,
        })
    })
}

fn presentation_message(actor: &str, message: &WireMessage) -> Option<ActorCommand> {
    serde_json::to_vec(message).ok().map(|body| {
        ActorCommand::SendPresentationMessage(PresentationActorMessage {
            actor: actor.to_string(),
            body,
        })
    })
}

fn session_message(session_id: u64, actor: &str, message: &WireMessage) -> Option<ActorCommand> {
    serde_json::to_vec(message).ok().map(|body| {
        ActorCommand::SendSessionMessage(SessionActorMessage {
            session_id,
            actor: actor.to_string(),
            body,
        })
    })
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
                handle_ui_key(ui, &key.logical_key, key.pressed),
                should_redraw,
            )
        }
        PresentationEvent::Text(text) if text.pane == ui.pane => {
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
        PresentationEvent::PointerButton(button)
            if button.pane == ui.pane && button.pressed && button.button == LEFT_BUTTON =>
        {
            (handle_ui_click(ui, button.x, button.y), true)
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
        if let Some(command) = subscribe_agent(ui.session_id, agent) {
            commands.push(command);
        }
    }
    commands
}

fn subscribe_agent(session_id: u64, agent: &str) -> Option<ActorCommand> {
    session_message(
        session_id,
        agent,
        &WireMessage {
            kind: "subscribe".to_string(),
            agent: agent.to_string(),
            ..WireMessage::default()
        },
    )
}

fn handle_ui_key(ui: &mut UiState, key: &str, pressed: bool) -> Vec<ActorCommand> {
    if pressed {
        if key == "Backspace" {
            ui.prompt.pop();
        }
        return Vec::new();
    }
    match key {
        "Enter" | "Return" | "NumpadEnter" => submit_prompt(ui).into_iter().collect(),
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

fn handle_ui_click(ui: &mut UiState, x: f32, y: f32) -> Vec<ActorCommand> {
    if y < 44.0 {
        let index = ((x - 12.0) / 164.0).floor().max(0.0) as usize;
        if index < ui.agents.len() {
            ui.selected = index;
        }
        return Vec::new();
    }
    if y > ui.height - 64.0 && x > ui.width - 112.0 {
        return submit_prompt(ui).into_iter().collect();
    }
    Vec::new()
}

fn submit_prompt(ui: &mut UiState) -> Option<ActorCommand> {
    let agent = ui.selected_agent()?.to_string();
    if ui.prompt.trim().is_empty() || ui.session_id == 0 {
        return None;
    }
    let view = ui.views.get(&agent)?;
    let prompt = std::mem::take(&mut ui.prompt);
    session_message(
        ui.session_id,
        &agent,
        &WireMessage {
            kind: "prompt".to_string(),
            agent: agent.clone(),
            prompt,
            model: view.model.clone(),
            reasoning: view.reasoning.clone(),
            ..WireMessage::default()
        },
    )
}

fn create_agent(ui: &UiState) -> Option<ActorCommand> {
    session_message(
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
            packet
                .encode()
                .map_err(|error| format!("encode draw packet: {error}"))
                .and_then(|bytes| {
                    host::graphics_submit_draw_packet(ui.pane, &bytes)
                        .map(|_| ())
                        .map_err(|error| format!("submit draw packet: {error:?}"))
                })
        },
        Err,
    );
    match submitted {
        Ok(()) => {
            let stale = ui
                .text_resources
                .keys()
                .filter(|key| !ui.frame_text_resources.contains(*key))
                .cloned()
                .collect::<Vec<_>>();
            for key in stale {
                if let Some(resource) = ui.text_resources.remove(&key) {
                    let _ = host::graphics_drop_resource(ui.pane, resource);
                }
            }
            ui.created_text_resources.clear();
            ui.error.clear();
        }
        Err(error) => {
            for key in std::mem::take(&mut ui.created_text_resources) {
                if let Some(resource) = ui.text_resources.remove(&key) {
                    let _ = host::graphics_drop_resource(ui.pane, resource);
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
    let tab_width = if agents.is_empty() {
        152.0
    } else {
        ((ui.width - 24.0) / agents.len() as f32).clamp(48.0, 152.0)
    };
    for (index, agent) in agents.iter().enumerate() {
        let x = 12.0 + index as f32 * (tab_width + 4.0);
        packet.push(Command::FillRoundedRect {
            rect: Rect::new(x, 8.0, tab_width, 28.0),
            radius: 5.0,
            color: if index == ui.selected {
                BLUE
            } else {
                PANEL_LIGHT
            },
        });
        draw_text(
            ui,
            packet,
            agent,
            x + 7.0,
            12.0,
            14.0,
            (tab_width - 14.0).max(24.0),
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
    draw_text(
        ui,
        packet,
        &format!("status  {status}"),
        16.0,
        59.0,
        14.0,
        190.0,
        if status == "running" { GREEN } else { FG },
    );
    draw_text(
        ui,
        packet,
        &format!("F6 model  {model}"),
        220.0,
        59.0,
        14.0,
        240.0,
        BLUE,
    );
    draw_text(
        ui,
        packet,
        &format!("F7 reasoning  {reasoning}"),
        480.0,
        59.0,
        14.0,
        260.0,
        YELLOW,
    );
}

fn draw_transcript(ui: &mut UiState, packet: &mut Packet) {
    let lines = ui
        .selected_agent()
        .and_then(|agent| ui.views.get(agent))
        .map(|view| view.logs.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let available = ((ui.height - 190.0) / 24.0).max(1.0) as usize;
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
    let y = ui.height - 78.0;
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(12.0, y, ui.width - 24.0, 60.0),
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
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(ui.width - 104.0, y + 10.0, 80.0, 38.0),
        radius: 7.0,
        color: GREEN,
    });
    draw_text(
        ui,
        packet,
        "Enter",
        ui.width - 88.0,
        y + 19.0,
        14.0,
        56.0,
        PANEL,
    );
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
        let layout = match host::graphics_layout_text(
            ui.pane,
            text,
            host::TextLayoutOptions {
                font_intent: host::FontIntent::Ui,
                font_weight: host::FontWeight::Normal,
                font_style: host::FontStyle::Normal,
                font_size: size,
                max_width: width,
                line_height: size * 1.35,
                red: color.r,
                green: color.g,
                blue: color.b,
                alpha: color.a,
            },
        ) {
            Ok(layout) => layout,
            Err(error) => {
                ui.draw_error
                    .get_or_insert_with(|| format!("layout text: {error:?}"));
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

async fn run_codex_turn(request: &TurnRequest) -> Result<Vec<u8>, String> {
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
    arguments.push(request.prompt.clone());
    let process = host::subprocess_spawn("codex", &arguments)
        .map_err(|error| format!("spawn codex: {error:?}"))?;
    let _ = host::stream_close_write(process.stdin);
    let result = drain_codex_process(&request.agent, &process).await;
    let _ = host::stream_drop(process.stdin).await;
    let _ = host::stream_drop(process.stdout).await;
    let _ = host::stream_drop(process.stderr).await;
    let _ = host::child_drop(process.child).await;
    result
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

export!(CodingAgentPlugin);
