//! Constructors for ordinary actor commands.

use crate::exports::brood::plugin::actor::{
    ActorStartRequest, Command, DaemonActorMessage, LocalActorMessage, PresentationActorMessage,
    PresentationScope, PresentationSession, SessionActorMessage, WorkerRequest,
};

/// Start a presentation-global child actor, or an ordinary same-realm child
/// when used by a backend init actor.
#[must_use]
pub fn start_global_actor(
    id: impl Into<String>,
    path: impl Into<String>,
    capabilities: Vec<String>,
) -> Command {
    Command::StartActor(ActorStartRequest {
        id: id.into(),
        path: path.into(),
        capabilities,
        presentation_scope: PresentationScope::Global,
    })
}

/// Start a presentation child whose pane remains scoped to one backend
/// session across active-session changes.
#[must_use]
pub fn start_session_actor(
    id: impl Into<String>,
    path: impl Into<String>,
    capabilities: Vec<String>,
    backend_id: u64,
    session_id: u64,
) -> Command {
    Command::StartActor(ActorStartRequest {
        id: id.into(),
        path: path.into(),
        capabilities,
        presentation_scope: PresentationScope::Session(PresentationSession {
            backend_id,
            session_id,
        }),
    })
}

/// Enqueue work without blocking the actor dispatcher.
#[must_use]
pub const fn spawn_worker(worker: u64, payload: Vec<u8>) -> Command {
    Command::SpawnWorker(WorkerRequest { worker, payload })
}

/// Send to another actor in the current realm.
#[must_use]
pub fn send_local(actor: impl Into<String>, body: impl Into<Vec<u8>>) -> Command {
    Command::SendLocalMessage(LocalActorMessage {
        actor: actor.into(),
        body: body.into(),
    })
}

/// Send from a session actor to its attached presentation realm.
#[must_use]
pub fn send_presentation(actor: impl Into<String>, body: impl Into<Vec<u8>>) -> Command {
    Command::SendPresentationMessage(PresentationActorMessage {
        actor: actor.into(),
        body: body.into(),
    })
}

/// Send from a presentation actor to one surface-session actor.
#[must_use]
pub fn send_session(
    backend_id: u64,
    session_id: u64,
    actor: impl Into<String>,
    body: impl Into<Vec<u8>>,
) -> Command {
    Command::SendSessionMessage(SessionActorMessage {
        backend_id,
        session_id,
        actor: actor.into(),
        body: body.into(),
    })
}

/// Send from a presentation actor to one daemon actor.
#[must_use]
pub fn send_daemon(backend_id: u64, actor: impl Into<String>, body: impl Into<Vec<u8>>) -> Command {
    Command::SendDaemonMessage(DaemonActorMessage {
        backend_id,
        actor: actor.into(),
        body: body.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_message_preserves_explicit_address_and_bytes() {
        let message = match send_local("picker", b"open".to_vec()) {
            Command::SendLocalMessage(message) => Some(message),
            _ => None,
        };
        assert!(message.is_some());
        if let Some(message) = message {
            assert_eq!(message.actor, "picker");
            assert_eq!(message.body, b"open");
        }
    }

    #[test]
    fn session_message_keeps_backend_qualified_identity() {
        let message = match send_session(7, 11, "terminal-session", b"input".to_vec()) {
            Command::SendSessionMessage(message) => Some(message),
            _ => None,
        };
        assert!(message.is_some());
        if let Some(message) = message {
            assert_eq!((message.backend_id, message.session_id), (7, 11));
            assert_eq!(message.actor, "terminal-session");
        }
    }

    #[test]
    fn actor_start_scope_distinguishes_global_and_session_panes() {
        let global = match start_global_actor("inbox", "inbox.wasm", Vec::new()) {
            Command::StartActor(request) => Some(request),
            _ => None,
        };
        assert!(global.is_some());
        if let Some(global) = global {
            assert!(matches!(
                global.presentation_scope,
                PresentationScope::Global
            ));
        }

        let scoped = match start_session_actor("terminal", "terminal.wasm", Vec::new(), 3, 9) {
            Command::StartActor(request) => Some(request),
            _ => None,
        };
        assert!(scoped.is_some());
        if let Some(scoped) = scoped {
            assert!(matches!(
                scoped.presentation_scope,
                PresentationScope::Session(PresentationSession {
                    backend_id: 3,
                    session_id: 9,
                })
            ));
        }
    }
}
