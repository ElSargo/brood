wit_bindgen::generate!({
    path: "../../crates/manifold-wasm/wit",
    world: "manifold-plugin",
});

use exports::manifold::plugin::actor::{
    AppStartRequest, Command, Event, Guest, LifecycleEvent, SessionActorMessage,
};

const CROSS_REALM_PROOF: &[u8] = b"client-to-session-actor-proof";

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
                vec![
                    Command::SendSessionMessage(SessionActorMessage {
                        session_id,
                        actor: "init".to_string(),
                        body: CROSS_REALM_PROOF.to_vec(),
                    }),
                    Command::LifecycleComplete(Ok(())),
                ]
            }
            Event::Lifecycle(LifecycleEvent::Start(_)) => vec![
                Command::StartApp(AppStartRequest {
                    id: "terminal".to_string(),
                    argv: vec!["foot".to_string()],
                }),
                Command::LifecycleComplete(Ok(())),
            ],
            Event::Mailbox(message)
                if message.sender == "presentation:manifold.client-init"
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

export!(ExampleInitActor with_types_in self);
