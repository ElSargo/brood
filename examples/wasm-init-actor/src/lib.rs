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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct WireMessage {
    kind: String,
    session_id: u64,
    agents: Vec<WireAgent>,
    terminal: Option<String>,
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
}

struct InitState {
    session_id: u64,
    agents: Vec<WireAgent>,
    subscribers: Vec<Subscriber>,
    next_agent: u64,
    next_terminal: u64,
    pending_terminals: Vec<PendingTerminal>,
}

static STATE: Mutex<InitState> = Mutex::new(InitState {
    session_id: 0,
    agents: Vec::new(),
    subscribers: Vec::new(),
    next_agent: 2,
    next_terminal: 2,
    pending_terminals: Vec::new(),
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
                return terminal_reply(
                    &pending.subscriber,
                    "terminal-started",
                    &pending.id,
                    state.session_id,
                );
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
            let id = format!("terminal-session-{}", state.next_terminal);
            state.next_terminal = state.next_terminal.saturating_add(1);
            state.pending_terminals.push(PendingTerminal {
                id: id.clone(),
                subscriber: subscriber_from_sender(sender),
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
        terminal: None,
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
        agents: Vec::new(),
        terminal: Some(terminal.to_string()),
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

export!(ExampleInitActor with_types_in self);
