wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use exports::brood::plugin::actor::{
    ActorStartRequest, AppStartRequest, ChildEvent, Command, Event, Guest, LifecycleEvent,
    LocalActorMessage, PresentationActorMessage, SessionActorMessage,
};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

const CROSS_REALM_PROOF: &[u8] = b"client-to-session-actor-proof";
const CODING_AGENT_COMPONENT_FILE: &str = "brood_coding_agent_plugin.component.wasm";
const TERMINAL_SESSION_COMPONENT_FILE: &str = "brood_terminal_session_plugin.component.wasm";
const FIRST_AGENT: &str = "coding-agent-1";
const NEOVIM_SESSION: &str = "neovim-session";
const MAX_TERMINAL_COMMAND_BYTES: usize = 8 * 1024;
const MAX_EDITOR_PATH_BYTES: usize = 4 * 1024;
const MAX_PENDING_EDITORS: usize = 32;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct WireMessage {
    kind: String,
    session_id: u64,
    agents: Vec<WireAgent>,
    terminal: Option<String>,
    editor: Option<String>,
    command: String,
    message: String,
    path: String,
    line: Option<u32>,
    column: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct WireAgent {
    id: String,
    status: String,
    model: String,
    reasoning: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Subscriber {
    Local(String),
    Presentation(String),
}

struct PendingTerminal {
    id: String,
    subscriber: Subscriber,
    command: String,
}

struct PendingEditor {
    subscriber: Subscriber,
    open: Option<EditorOpen>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct EditorOpen {
    kind: String,
    path: String,
    line: Option<u32>,
    column: Option<u32>,
}

struct InitState {
    session_id: u64,
    agents: Vec<WireAgent>,
    subscribers: Vec<Subscriber>,
    next_agent: u64,
    next_terminal: u64,
    pending_terminals: Vec<PendingTerminal>,
    editor_actor_started: bool,
    editor_started: bool,
    pending_editors: Vec<PendingEditor>,
}

static STATE: Mutex<InitState> = Mutex::new(InitState {
    session_id: 0,
    agents: Vec::new(),
    subscribers: Vec::new(),
    next_agent: 2,
    next_terminal: 2,
    pending_terminals: Vec::new(),
    editor_actor_started: false,
    editor_started: false,
    pending_editors: Vec::new(),
});

struct ExampleInitActor;

impl Guest for ExampleInitActor {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(context))
                if context
                    .data
                    .iter()
                    .any(|entry| entry.key == "realm" && entry.value == "client") =>
            {
                let Some(session_id) = context.session_id else {
                    return vec![Command::LifecycleComplete(Err(
                        "client init actor has no bound session".to_string(),
                    ))];
                };
                let Some(backend_id) = context.backend_id else {
                    return vec![Command::LifecycleComplete(Err(
                        "client init actor has no bound backend".to_string(),
                    ))];
                };
                vec![
                    Command::SendSessionMessage(SessionActorMessage {
                        backend_id,
                        session_id,
                        actor: "init".to_string(),
                        body: CROSS_REALM_PROOF.to_vec(),
                    }),
                    Command::LifecycleComplete(Ok(())),
                ]
            }
            Event::Lifecycle(LifecycleEvent::Start(context)) => {
                if let Ok(mut state) = STATE.lock() {
                    state.session_id = context.session_id.unwrap_or_default();
                }
                vec![
                    start_terminal("terminal-session"),
                    start_agent(FIRST_AGENT),
                    Command::LifecycleComplete(Ok(())),
                ]
            }
            Event::Child(event) => handle_child(event),
            Event::Mailbox(message)
                if message.sender == "presentation:brood.client-init"
                    && message.body == CROSS_REALM_PROOF =>
            {
                vec![Command::StartApp(AppStartRequest {
                    id: "client-to-session-proof".to_string(),
                    argv: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "exit 0".to_string(),
                        "client-to-session-actor-proof".to_string(),
                    ],
                })]
            }
            Event::Mailbox(message) => handle_mailbox(&message.sender, &message.body),
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![Command::LifecycleComplete(Ok(()))]
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

fn start_agent(id: &str) -> Command {
    Command::StartActor(ActorStartRequest {
        id: id.to_string(),
        path: CODING_AGENT_COMPONENT_FILE.to_string(),
        capabilities: vec![
            "filesystem:workspace-write".to_string(),
            "subprocess:codex".to_string(),
        ],
        backend_id: None,
        session_id: None,
    })
}

fn start_terminal(id: &str) -> Command {
    Command::StartActor(ActorStartRequest {
        id: id.to_string(),
        path: TERMINAL_SESSION_COMPONENT_FILE.to_string(),
        capabilities: vec!["pty".to_string(), "subprocess:sh".to_string()],
        backend_id: None,
        session_id: None,
    })
}

fn start_editor() -> Command {
    Command::StartActor(ActorStartRequest {
        id: NEOVIM_SESSION.to_string(),
        path: TERMINAL_SESSION_COMPONENT_FILE.to_string(),
        capabilities: vec![
            "pty".to_string(),
            "subprocess:brood-nvim".to_string(),
            "subprocess:nvim".to_string(),
        ],
        backend_id: None,
        session_id: None,
    })
}

fn handle_child(event: ChildEvent) -> Vec<Command> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match event {
        ChildEvent::Started(started) => {
            if let Some(index) = state
                .pending_terminals
                .iter()
                .position(|pending| pending.id == started.id)
            {
                let pending = state.pending_terminals.remove(index);
                let mut commands = Vec::new();
                if let Some(body) = terminal_command_body(&pending.command) {
                    commands.push(Command::SendLocalMessage(LocalActorMessage {
                        actor: pending.id.clone(),
                        body,
                    }));
                }
                commands.extend(terminal_reply(
                    &pending.subscriber,
                    "terminal-started",
                    &pending.id,
                    state.session_id,
                ));
                return commands;
            }
            if started.id == NEOVIM_SESSION {
                state.editor_actor_started = true;
                return Vec::new();
            }
            if !started.id.starts_with("coding-agent-") {
                return Vec::new();
            }
            if !state.agents.iter().any(|agent| agent.id == started.id) {
                state.agents.push(WireAgent {
                    id: started.id,
                    status: "idle".to_string(),
                    model: "default".to_string(),
                    reasoning: "medium".to_string(),
                });
            }
        }
        ChildEvent::Stopped(id) => {
            if id == NEOVIM_SESSION {
                state.editor_actor_started = false;
                state.editor_started = false;
                if !state.pending_editors.is_empty() {
                    state.editor_actor_started = true;
                    return vec![start_editor()];
                }
                return Vec::new();
            }
            if !id.starts_with("coding-agent-") {
                return Vec::new();
            }
            state.agents.retain(|agent| agent.id != id);
        }
        ChildEvent::Failed(failed) => {
            if let Some(index) = state
                .pending_terminals
                .iter()
                .position(|pending| pending.id == failed.id)
            {
                let pending = state.pending_terminals.remove(index);
                return terminal_reply(
                    &pending.subscriber,
                    "terminal-failed",
                    &pending.id,
                    state.session_id,
                );
            }
            if failed.id == NEOVIM_SESSION {
                state.editor_actor_started = false;
                state.editor_started = false;
                let session_id = state.session_id;
                return std::mem::take(&mut state.pending_editors)
                    .into_iter()
                    .flat_map(|pending| {
                        editor_reply(&pending.subscriber, "editor-failed", session_id)
                    })
                    .collect();
            }
            if !failed.id.starts_with("coding-agent-") {
                return Vec::new();
            }
            if let Some(agent) = state.agents.iter_mut().find(|agent| agent.id == failed.id) {
                agent.status = format!("failed: {}", failed.message);
            }
        }
    }
    registry_commands(&state)
}

fn handle_mailbox(sender: &str, body: &[u8]) -> Vec<Command> {
    let Ok(message) = serde_json::from_slice::<WireMessage>(body) else {
        return Vec::new();
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    if sender == NEOVIM_SESSION {
        match message.kind.as_str() {
            "editor-ready" => {
                state.editor_actor_started = true;
                state.editor_started = true;
                return complete_pending_editors(&mut state);
            }
            "editor-stopped" => {
                state.editor_started = false;
                return vec![Command::StopActor(NEOVIM_SESSION.to_string())];
            }
            "editor-failed" => {
                state.editor_started = false;
                let session_id = state.session_id;
                return std::mem::take(&mut state.pending_editors)
                    .into_iter()
                    .flat_map(|pending| {
                        editor_reply_with_message(
                            &pending.subscriber,
                            "editor-failed",
                            session_id,
                            &message.message,
                        )
                    })
                    .chain(std::iter::once(Command::StopActor(
                        NEOVIM_SESSION.to_string(),
                    )))
                    .collect();
            }
            _ => {}
        }
    }
    match message.kind.as_str() {
        "registry-subscribe" => {
            let subscriber = subscriber_from_sender(sender);
            if !state.subscribers.contains(&subscriber) {
                state.subscribers.push(subscriber);
            }
            registry_commands(&state)
        }
        "create-agent" => {
            let id = format!("coding-agent-{}", state.next_agent);
            state.next_agent = state.next_agent.saturating_add(1);
            vec![start_agent(&id)]
        }
        "create-terminal" => {
            if message.command.len() > MAX_TERMINAL_COMMAND_BYTES || message.command.contains('\0')
            {
                return Vec::new();
            }
            let id = format!("terminal-session-{}", state.next_terminal);
            state.next_terminal = state.next_terminal.saturating_add(1);
            state.pending_terminals.push(PendingTerminal {
                id: id.clone(),
                subscriber: subscriber_from_sender(sender),
                command: message.command,
            });
            vec![start_terminal(&id)]
        }
        "stop-terminal" => message
            .terminal
            .filter(|terminal| {
                terminal == "terminal-session" || terminal.starts_with("terminal-session-")
            })
            .map(Command::StopActor)
            .into_iter()
            .collect(),
        "create-editor" | "editor-status" | "open-editor" => {
            let subscriber = subscriber_from_sender(sender);
            let open = (message.kind == "open-editor")
                .then(|| EditorOpen {
                    kind: "editor-open".to_string(),
                    path: message.path,
                    line: message.line,
                    column: message.column,
                })
                .filter(valid_editor_open);
            if message.kind == "open-editor" && open.is_none() {
                return Vec::new();
            }
            if state.editor_started {
                let mut commands = open
                    .and_then(editor_open_command)
                    .into_iter()
                    .collect::<Vec<_>>();
                commands.extend(editor_reply(
                    &subscriber,
                    if message.kind == "editor-status" {
                        "editor-present"
                    } else {
                        "editor-started"
                    },
                    state.session_id,
                ));
                return commands;
            }
            if message.kind == "editor-status" {
                return Vec::new();
            }
            if state.pending_editors.len() >= MAX_PENDING_EDITORS {
                return editor_reply_with_message(
                    &subscriber,
                    "editor-failed",
                    state.session_id,
                    "editor startup request queue reached its bounded capacity",
                );
            }
            state
                .pending_editors
                .push(PendingEditor { subscriber, open });
            (!state.editor_actor_started)
                .then(|| {
                    state.editor_actor_started = true;
                    start_editor()
                })
                .into_iter()
                .collect()
        }
        "stop-editor" => {
            state.editor_actor_started = false;
            state.editor_started = false;
            state.pending_editors.clear();
            vec![Command::StopActor(NEOVIM_SESSION.to_string())]
        }
        _ => Vec::new(),
    }
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

fn registry_commands(state: &InitState) -> Vec<Command> {
    let Ok(body) = serde_json::to_vec(&WireMessage {
        kind: "registry".to_string(),
        session_id: state.session_id,
        agents: state.agents.clone(),
        ..WireMessage::default()
    }) else {
        return Vec::new();
    };
    state
        .subscribers
        .iter()
        .map(|subscriber| match subscriber {
            Subscriber::Local(actor) => Command::SendLocalMessage(LocalActorMessage {
                actor: actor.clone(),
                body: body.clone(),
            }),
            Subscriber::Presentation(actor) => {
                Command::SendPresentationMessage(PresentationActorMessage {
                    actor: actor.clone(),
                    body: body.clone(),
                })
            }
        })
        .collect()
}

fn terminal_reply(
    subscriber: &Subscriber,
    kind: &str,
    terminal: &str,
    session_id: u64,
) -> Vec<Command> {
    let Ok(body) = serde_json::to_vec(&WireMessage {
        kind: kind.to_string(),
        session_id,
        terminal: Some(terminal.to_string()),
        ..WireMessage::default()
    }) else {
        return Vec::new();
    };
    vec![match subscriber {
        Subscriber::Local(actor) => Command::SendLocalMessage(LocalActorMessage {
            actor: actor.clone(),
            body,
        }),
        Subscriber::Presentation(actor) => {
            Command::SendPresentationMessage(PresentationActorMessage {
                actor: actor.clone(),
                body,
            })
        }
    }]
}

fn terminal_command_body(command: &str) -> Option<Vec<u8>> {
    if command.is_empty() {
        return None;
    }
    let mut body = Vec::with_capacity(command.len().saturating_add(10));
    body.push(b'I');
    body.extend_from_slice(&0_u64.to_le_bytes());
    body.extend_from_slice(command.as_bytes());
    if !command.ends_with('\n') {
        body.push(b'\n');
    }
    Some(body)
}

fn valid_editor_open(open: &EditorOpen) -> bool {
    !open.path.is_empty()
        && open.path.len() <= MAX_EDITOR_PATH_BYTES
        && !open.path.contains('\0')
        && open.line.unwrap_or(1) > 0
        && open.column.unwrap_or(1) > 0
}

fn editor_open_command(open: EditorOpen) -> Option<Command> {
    valid_editor_open(&open).then(|| {
        serde_json::to_vec(&open).ok().map(|body| {
            Command::SendLocalMessage(LocalActorMessage {
                actor: NEOVIM_SESSION.to_string(),
                body,
            })
        })
    })?
}

fn complete_pending_editors(state: &mut InitState) -> Vec<Command> {
    let session_id = state.session_id;
    std::mem::take(&mut state.pending_editors)
        .into_iter()
        .flat_map(|pending| {
            let mut commands = pending
                .open
                .and_then(editor_open_command)
                .into_iter()
                .collect::<Vec<_>>();
            commands.extend(editor_reply(
                &pending.subscriber,
                "editor-started",
                session_id,
            ));
            commands
        })
        .collect()
}

fn editor_reply(subscriber: &Subscriber, kind: &str, session_id: u64) -> Vec<Command> {
    editor_reply_with_message(subscriber, kind, session_id, "")
}

fn editor_reply_with_message(
    subscriber: &Subscriber,
    kind: &str,
    session_id: u64,
    message: &str,
) -> Vec<Command> {
    let Ok(body) = serde_json::to_vec(&WireMessage {
        kind: kind.to_string(),
        session_id,
        editor: Some(NEOVIM_SESSION.to_string()),
        message: message.chars().take(2_048).collect(),
        ..WireMessage::default()
    }) else {
        return Vec::new();
    };
    vec![match subscriber {
        Subscriber::Local(actor) => Command::SendLocalMessage(LocalActorMessage {
            actor: actor.clone(),
            body,
        }),
        Subscriber::Presentation(actor) => {
            Command::SendPresentationMessage(PresentationActorMessage {
                actor: actor.clone(),
                body,
            })
        }
    }]
}

#[cfg(test)]
mod tests {
    use super::{EditorOpen, terminal_command_body, valid_editor_open};

    #[test]
    fn terminal_command_is_one_sequenced_input_message() {
        let body = terminal_command_body("cargo test").expect("terminal command");
        assert_eq!(body[0], b'I');
        assert_eq!(&body[1..9], &0_u64.to_le_bytes());
        assert_eq!(&body[9..], b"cargo test\n");
    }

    #[test]
    fn editor_open_requires_a_bounded_positive_location() {
        assert!(valid_editor_open(&EditorOpen {
            kind: "editor-open".to_string(),
            path: "src/main.rs".to_string(),
            line: Some(4),
            column: Some(2),
        }));
        assert!(!valid_editor_open(&EditorOpen {
            kind: "editor-open".to_string(),
            path: "../\0escape".to_string(),
            line: Some(0),
            column: None,
        }));
    }
}

export!(ExampleInitActor with_types_in self);
