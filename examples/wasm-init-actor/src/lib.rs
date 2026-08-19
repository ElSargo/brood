wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use exports::brood::plugin::actor::{
    ActorStartRequest, AppStartRequest, ChildEvent, Command, Event, Guest, LifecycleEvent,
    LocalActorMessage, PresentationActorMessage, PresentationScope, SessionActorMessage,
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Component, Path},
    sync::Mutex,
};

const CROSS_REALM_PROOF: &[u8] = b"client-to-session-actor-proof";
const CODING_AGENT_COMPONENT_FILE: &str = "brood_coding_agent_plugin.component.wasm";
const TERMINAL_SESSION_COMPONENT_FILE: &str = "brood_terminal_session_plugin.component.wasm";
const NEOVIM_FILES_COMPONENT_FILE: &str = "brood_neovim_files_plugin.component.wasm";
const FIRST_AGENT: &str = "coding-agent-1";
const NEOVIM_FILES: &str = "neovim-files";
const NEOVIM_CLIENT_PREFIX: &str = "neovim-ui-b";
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
    actor: String,
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

#[derive(Serialize)]
struct EditorFileControl<'a> {
    kind: &'a str,
    session_id: u64,
    actor: &'a str,
}

struct InitState {
    session_id: u64,
    agents: Vec<WireAgent>,
    subscribers: Vec<Subscriber>,
    next_agent: u64,
    next_terminal: u64,
    pending_terminals: Vec<PendingTerminal>,
    editor_files_starting: bool,
    editor_files_ready: bool,
    editor_client: Option<String>,
    editor_client_files_ready: bool,
    pending_editors: Vec<PendingEditor>,
    pending_opens: Vec<EditorOpen>,
}

static STATE: Mutex<InitState> = Mutex::new(InitState {
    session_id: 0,
    agents: Vec::new(),
    subscribers: Vec::new(),
    next_agent: 2,
    next_terminal: 2,
    pending_terminals: Vec::new(),
    editor_files_starting: false,
    editor_files_ready: false,
    editor_client: None,
    editor_client_files_ready: false,
    pending_editors: Vec::new(),
    pending_opens: Vec::new(),
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
                    state.editor_files_starting = true;
                }
                vec![
                    start_terminal("terminal-session"),
                    start_agent(FIRST_AGENT),
                    start_editor_files(),
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
            "subprocess:brood-agent".to_string(),
        ],
        presentation_scope: PresentationScope::Global,
    })
}

fn start_terminal(id: &str) -> Command {
    Command::StartActor(ActorStartRequest {
        id: id.to_string(),
        path: TERMINAL_SESSION_COMPONENT_FILE.to_string(),
        capabilities: vec!["pty".to_string(), "subprocess:sh".to_string()],
        presentation_scope: PresentationScope::Global,
    })
}

fn start_editor_files() -> Command {
    Command::StartActor(ActorStartRequest {
        id: NEOVIM_FILES.to_string(),
        path: NEOVIM_FILES_COMPONENT_FILE.to_string(),
        capabilities: vec!["filesystem:workspace-write".to_string()],
        presentation_scope: PresentationScope::Global,
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
            if started.id == NEOVIM_FILES {
                state.editor_files_starting = false;
                state.editor_files_ready = true;
                state.editor_client_files_ready = false;
                let mut commands = complete_pending_editors(&mut state);
                if let Some(client) = state.editor_client.as_deref()
                    && let Some(bind) = editor_file_control("bind-client", state.session_id, client)
                {
                    commands.push(bind);
                }
                return commands;
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
            if id == NEOVIM_FILES {
                state.editor_files_starting = false;
                state.editor_files_ready = false;
                state.editor_client = None;
                state.editor_client_files_ready = false;
                return fail_pending_editors(&mut state, "remote editor file service stopped");
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
            if failed.id == NEOVIM_FILES {
                state.editor_files_starting = false;
                state.editor_files_ready = false;
                state.editor_client = None;
                state.editor_client_files_ready = false;
                return fail_pending_editors(&mut state, &failed.message);
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
    // The backend host creates presentation senders after checking the
    // emitting actor's realm association. Client-init is additionally trusted
    // to report lifecycle failure for one of its exact, deterministic editor
    // children when that child can no longer report for itself.
    let direct_client = sender
        .strip_prefix("presentation:")
        .filter(|client| valid_editor_client_actor(client, state.session_id));
    let lifecycle_client = (sender == "presentation:brood.client-init"
        && matches!(
            message.kind.as_str(),
            "editor-client-stopped" | "editor-client-failed"
        )
        && valid_editor_client_actor(&message.actor, state.session_id))
    .then_some(message.actor.as_str());
    if let Some(client) = direct_client.or(lifecycle_client) {
        match message.kind.as_str() {
            "editor-client-ready"
                if message.session_id == state.session_id && message.actor == client =>
            {
                let mut commands = state
                    .editor_client
                    .as_deref()
                    .filter(|previous| *previous != client)
                    .and_then(|previous| {
                        editor_file_control("revoke-client", state.session_id, previous)
                    })
                    .into_iter()
                    .collect::<Vec<_>>();
                state.editor_client = Some(client.to_string());
                state.editor_client_files_ready = false;
                if state.editor_files_ready
                    && let Some(bind) = editor_file_control("bind-client", state.session_id, client)
                {
                    commands.push(bind);
                }
                return commands;
            }
            "editor-files-ready"
                if message.session_id == state.session_id
                    && message.actor == client
                    && state.editor_files_ready
                    && state.editor_client.as_deref() == Some(client) =>
            {
                state.editor_client_files_ready = true;
                return flush_pending_opens(&mut state);
            }
            "editor-client-stopped"
                if message.session_id == state.session_id
                    && message.actor == client
                    && state.editor_client.as_deref() == Some(client) =>
            {
                let revoke = editor_file_control("revoke-client", state.session_id, client);
                state.editor_client = None;
                state.editor_client_files_ready = false;
                let mut commands = revoke.into_iter().collect::<Vec<_>>();
                if state.pending_opens.is_empty() && state.pending_editors.is_empty() {
                    commands.extend(editor_stop(state.session_id));
                } else {
                    commands.extend(fail_pending_editors(
                        &mut state,
                        "local editor client stopped before queued opens were delivered",
                    ));
                }
                return commands;
            }
            "editor-client-failed"
                if message.session_id == state.session_id
                    && message.actor == client
                    && state
                        .editor_client
                        .as_deref()
                        .is_none_or(|current| current == client) =>
            {
                let revoke = state.editor_client.as_deref().and_then(|current| {
                    editor_file_control("revoke-client", state.session_id, current)
                });
                state.editor_client = None;
                state.editor_client_files_ready = false;
                let mut commands = revoke.into_iter().collect::<Vec<_>>();
                commands.extend(fail_pending_editors(&mut state, &message.message));
                return commands;
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
            if state.editor_files_ready {
                if message.kind == "editor-status" && !state.editor_client_files_ready {
                    return Vec::new();
                }
                if open.is_some()
                    && !state.editor_client_files_ready
                    && state.pending_opens.len() >= MAX_PENDING_EDITORS
                {
                    return editor_reply_with_message(
                        &subscriber,
                        "editor-failed",
                        state.session_id,
                        "editor open queue reached its bounded capacity",
                    );
                }
                let mut commands = open
                    .map(|open| queue_or_send_editor_open(&mut state, open))
                    .unwrap_or_default();
                commands.extend(editor_reply(
                    &subscriber,
                    if message.kind == "editor-status" {
                        "editor-present"
                    } else {
                        "editor-started"
                    },
                    state.session_id,
                ));
                if message.kind != "editor-status" && state.editor_client.is_none() {
                    commands.extend(editor_required(state.session_id));
                }
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
            (!state.editor_files_starting)
                .then(|| {
                    state.editor_files_starting = true;
                    start_editor_files()
                })
                .into_iter()
                .collect()
        }
        "stop-editor" => {
            let revoke = state
                .editor_client
                .as_deref()
                .and_then(|client| editor_file_control("revoke-client", state.session_id, client));
            state.editor_client = None;
            state.editor_client_files_ready = false;
            state.pending_editors.clear();
            state.pending_opens.clear();
            revoke
                .into_iter()
                .chain(editor_stop(state.session_id))
                .collect()
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

fn valid_editor_client_actor(actor: &str, session_id: u64) -> bool {
    let Some(address) = actor.strip_prefix(NEOVIM_CLIENT_PREFIX) else {
        return false;
    };
    let Some((backend, session)) = address.split_once("-s") else {
        return false;
    };
    backend.parse::<u64>().is_ok() && session.parse::<u64>().ok() == Some(session_id)
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
    let path = Path::new(&open.path);
    !open.path.is_empty()
        && open.path.len() <= MAX_EDITOR_PATH_BYTES
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && open.line.unwrap_or(1) > 0
        && open.column.unwrap_or(1) > 0
}

fn editor_open_command(actor: &str, open: &EditorOpen) -> Option<Command> {
    valid_editor_open(open).then(|| {
        serde_json::to_vec(open).ok().map(|body| {
            Command::SendPresentationMessage(PresentationActorMessage {
                actor: actor.to_string(),
                body,
            })
        })
    })?
}

fn editor_file_control(kind: &str, session_id: u64, actor: &str) -> Option<Command> {
    serde_json::to_vec(&EditorFileControl {
        kind,
        session_id,
        actor,
    })
    .ok()
    .map(|body| {
        Command::SendLocalMessage(LocalActorMessage {
            actor: NEOVIM_FILES.to_string(),
            body,
        })
    })
}

fn queue_or_send_editor_open(state: &mut InitState, open: EditorOpen) -> Vec<Command> {
    if state.editor_client_files_ready
        && let Some(actor) = state.editor_client.as_deref()
    {
        return editor_open_command(actor, &open).into_iter().collect();
    }
    if state.pending_opens.len() < MAX_PENDING_EDITORS {
        state.pending_opens.push(open);
    }
    Vec::new()
}

fn flush_pending_opens(state: &mut InitState) -> Vec<Command> {
    if !state.editor_client_files_ready {
        return Vec::new();
    }
    let Some(actor) = state.editor_client.clone() else {
        return Vec::new();
    };
    std::mem::take(&mut state.pending_opens)
        .iter()
        .filter_map(|open| editor_open_command(&actor, open))
        .collect()
}

fn complete_pending_editors(state: &mut InitState) -> Vec<Command> {
    let session_id = state.session_id;
    let pending = std::mem::take(&mut state.pending_editors);
    let needs_client = !pending.is_empty();
    let mut commands = pending
        .into_iter()
        .flat_map(|pending| {
            if let Some(open) = pending.open
                && state.pending_opens.len() < MAX_PENDING_EDITORS
            {
                state.pending_opens.push(open);
            }
            let mut replies = Vec::new();
            replies.extend(editor_reply(
                &pending.subscriber,
                "editor-started",
                session_id,
            ));
            replies
        })
        .collect::<Vec<_>>();
    if needs_client && state.editor_client.is_none() {
        commands.extend(editor_required(session_id));
    }
    commands
}

fn fail_pending_editors(state: &mut InitState, message: &str) -> Vec<Command> {
    let session_id = state.session_id;
    state.pending_opens.clear();
    std::mem::take(&mut state.pending_editors)
        .into_iter()
        .flat_map(|pending| {
            editor_reply_with_message(&pending.subscriber, "editor-failed", session_id, message)
        })
        .chain(editor_failed_for_client(session_id, message))
        .collect()
}

fn editor_required(session_id: u64) -> Vec<Command> {
    editor_presentation_notice("editor-started", session_id, "")
}

fn editor_stop(session_id: u64) -> Vec<Command> {
    editor_presentation_notice("editor-stop", session_id, "")
}

fn editor_failed_for_client(session_id: u64, message: &str) -> Vec<Command> {
    editor_presentation_notice("editor-failed", session_id, message)
}

fn editor_presentation_notice(kind: &str, session_id: u64, message: &str) -> Vec<Command> {
    let Ok(body) = serde_json::to_vec(&WireMessage {
        kind: kind.to_string(),
        session_id,
        editor: Some(NEOVIM_FILES.to_string()),
        message: message.chars().take(2_048).collect(),
        ..WireMessage::default()
    }) else {
        return Vec::new();
    };
    vec![Command::SendPresentationMessage(PresentationActorMessage {
        actor: "brood.client-init".to_string(),
        body,
    })]
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
        editor: Some(NEOVIM_FILES.to_string()),
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
    use super::{
        Command, EditorOpen, InitState, LocalActorMessage, MAX_PENDING_EDITORS, NEOVIM_FILES,
        PendingEditor, STATE, WireAgent, WireMessage, handle_mailbox, start_editor_files,
        terminal_command_body, valid_editor_client_actor, valid_editor_open,
    };
    use std::sync::Mutex;

    static STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset_state(session_id: u64) {
        *STATE.lock().expect("init state") = InitState {
            session_id,
            agents: Vec::<WireAgent>::new(),
            subscribers: Vec::new(),
            next_agent: 2,
            next_terminal: 2,
            pending_terminals: Vec::new(),
            editor_files_starting: false,
            editor_files_ready: true,
            editor_client: None,
            editor_client_files_ready: false,
            pending_editors: Vec::<PendingEditor>::with_capacity(MAX_PENDING_EDITORS),
            pending_opens: vec![EditorOpen {
                kind: "editor-open".to_string(),
                path: "src/main.rs".to_string(),
                line: Some(4),
                column: Some(2),
            }],
        };
    }

    fn wire(message: WireMessage) -> Vec<u8> {
        serde_json::to_vec(&message).expect("wire message")
    }

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
            path: "../escape".to_string(),
            line: Some(1),
            column: None,
        }));
        assert!(!valid_editor_open(&EditorOpen {
            kind: "editor-open".to_string(),
            path: "./main.rs".to_string(),
            line: Some(1),
            column: None,
        }));
        assert!(!valid_editor_open(&EditorOpen {
            kind: "editor-open".to_string(),
            path: "main.rs".to_string(),
            line: Some(0),
            column: None,
        }));
    }

    #[test]
    fn session_starts_only_the_remote_file_half_of_neovim() {
        let Command::StartActor(started) = start_editor_files() else {
            panic!("Neovim files actor was not started");
        };
        assert_eq!(started.id, "neovim-files");
        assert_eq!(started.path, "brood_neovim_files_plugin.component.wasm");
        assert_eq!(started.capabilities, ["filesystem:workspace-write"]);
    }

    #[test]
    fn editor_client_readiness_is_bound_to_this_session() {
        assert!(valid_editor_client_actor("neovim-ui-b4-s27", 27));
        assert!(!valid_editor_client_actor("neovim-ui-b4-s28", 27));
        assert!(!valid_editor_client_actor("other-b4-s27", 27));
        assert!(!valid_editor_client_actor("neovim-ui-b4-s27-extra", 27));
        assert!(!valid_editor_client_actor("neovim-ui-bevil-s27", 27));
    }

    #[test]
    fn editor_opens_wait_for_the_authenticated_file_binding_handshake() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state(27);
        let actor = "neovim-ui-b4-s27";

        let ready = handle_mailbox(
            &format!("presentation:{actor}"),
            &wire(WireMessage {
                kind: "editor-client-ready".to_string(),
                session_id: 27,
                actor: actor.to_string(),
                ..WireMessage::default()
            }),
        );
        let [
            Command::SendLocalMessage(LocalActorMessage {
                actor: recipient,
                body,
            }),
        ] = ready.as_slice()
        else {
            panic!("client readiness must bind through session init");
        };
        assert_eq!(recipient, NEOVIM_FILES);
        let bind: serde_json::Value = serde_json::from_slice(body).expect("bind JSON");
        assert_eq!(bind["kind"], "bind-client");
        assert_eq!(bind["session_id"], 27);
        assert_eq!(bind["actor"], actor);
        {
            let state = STATE.lock().expect("init state");
            assert!(!state.editor_client_files_ready);
            assert_eq!(state.pending_opens.len(), 1);
        }

        assert!(
            handle_mailbox(
                "presentation:neovim-ui-b5-s27",
                &wire(WireMessage {
                    kind: "editor-files-ready".to_string(),
                    session_id: 27,
                    actor: "neovim-ui-b5-s27".to_string(),
                    ..WireMessage::default()
                }),
            )
            .is_empty()
        );
        assert_eq!(STATE.lock().expect("init state").pending_opens.len(), 1);

        let opened = handle_mailbox(
            &format!("presentation:{actor}"),
            &wire(WireMessage {
                kind: "editor-files-ready".to_string(),
                session_id: 27,
                actor: actor.to_string(),
                ..WireMessage::default()
            }),
        );
        assert!(matches!(
            opened.as_slice(),
            [Command::SendPresentationMessage(message)]
                if message.actor == actor
        ));
        {
            let state = STATE.lock().expect("init state");
            assert!(state.editor_client_files_ready);
            assert!(state.pending_opens.is_empty());
        }

        let stopped = handle_mailbox(
            &format!("presentation:{actor}"),
            &wire(WireMessage {
                kind: "editor-client-stopped".to_string(),
                session_id: 27,
                actor: actor.to_string(),
                ..WireMessage::default()
            }),
        );
        let [
            Command::SendLocalMessage(revoke),
            Command::SendPresentationMessage(stop),
        ] = stopped.as_slice()
        else {
            panic!("clean client stop must revoke the file binding and close its presentation");
        };
        assert_eq!(revoke.actor, NEOVIM_FILES);
        let revoke: serde_json::Value = serde_json::from_slice(&revoke.body).expect("revoke JSON");
        assert_eq!(revoke["kind"], "revoke-client");
        assert_eq!(revoke["actor"], actor);
        let stop: serde_json::Value = serde_json::from_slice(&stop.body).expect("stop JSON");
        assert_eq!(stop["kind"], "editor-stop");
        assert!(STATE.lock().expect("init state").editor_client.is_none());
    }

    #[test]
    fn pre_ready_client_failure_clears_queued_opens_and_notifies_client_init() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state(27);
        let actor = "neovim-ui-b4-s27";

        let failed = handle_mailbox(
            &format!("presentation:{actor}"),
            &wire(WireMessage {
                kind: "editor-client-failed".to_string(),
                session_id: 27,
                actor: actor.to_string(),
                message: "runner exited before readiness".to_string(),
                ..WireMessage::default()
            }),
        );
        let [Command::SendPresentationMessage(notice)] = failed.as_slice() else {
            panic!("pre-ready failure must notify client init");
        };
        assert_eq!(notice.actor, "brood.client-init");
        let notice: serde_json::Value = serde_json::from_slice(&notice.body).expect("failure JSON");
        assert_eq!(notice["kind"], "editor-failed");
        assert_eq!(notice["message"], "runner exited before readiness");
        assert!(STATE.lock().expect("init state").pending_opens.is_empty());

        reset_state(27);
        STATE.lock().expect("init state").editor_client = Some(actor.to_string());
        assert!(
            handle_mailbox(
                "presentation:neovim-ui-b5-s27",
                &wire(WireMessage {
                    kind: "editor-client-failed".to_string(),
                    session_id: 27,
                    actor: "neovim-ui-b5-s27".to_string(),
                    message: "unrelated".to_string(),
                    ..WireMessage::default()
                }),
            )
            .is_empty()
        );
        assert_eq!(STATE.lock().expect("init state").pending_opens.len(), 1);

        reset_state(27);
        let failed = handle_mailbox(
            "presentation:brood.client-init",
            &wire(WireMessage {
                kind: "editor-client-failed".to_string(),
                session_id: 27,
                actor: actor.to_string(),
                message: "child failed before it could report".to_string(),
                ..WireMessage::default()
            }),
        );
        assert!(matches!(
            failed.as_slice(),
            [Command::SendPresentationMessage(notice)]
                if serde_json::from_slice::<serde_json::Value>(&notice.body)
                    .is_ok_and(|body| body["kind"] == "editor-failed")
        ));
        assert!(STATE.lock().expect("init state").pending_opens.is_empty());

        reset_state(27);
        assert!(
            handle_mailbox(
                "presentation:brood.client-init",
                &wire(WireMessage {
                    kind: "editor-client-failed".to_string(),
                    session_id: 27,
                    actor: "neovim-ui-b4-s27-spoof".to_string(),
                    message: "spoof".to_string(),
                    ..WireMessage::default()
                }),
            )
            .is_empty()
        );
        assert_eq!(STATE.lock().expect("init state").pending_opens.len(), 1);
    }

    #[test]
    fn client_stop_fails_only_opens_that_would_be_stranded() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state(27);
        let actor = "neovim-ui-b4-s27";
        STATE.lock().expect("init state").editor_client = Some(actor.to_string());

        let stopped = handle_mailbox(
            &format!("presentation:{actor}"),
            &wire(WireMessage {
                kind: "editor-client-stopped".to_string(),
                session_id: 27,
                actor: actor.to_string(),
                ..WireMessage::default()
            }),
        );
        let [
            Command::SendLocalMessage(_),
            Command::SendPresentationMessage(failure),
        ] = stopped.as_slice()
        else {
            panic!("a stop with queued opens must be reported as terminal failure");
        };
        let failure: serde_json::Value =
            serde_json::from_slice(&failure.body).expect("failure JSON");
        assert_eq!(failure["kind"], "editor-failed");
        let state = STATE.lock().expect("init state");
        assert!(state.pending_opens.is_empty());
        assert!(state.editor_client.is_none());
    }
}

export!(ExampleInitActor with_types_in self);
