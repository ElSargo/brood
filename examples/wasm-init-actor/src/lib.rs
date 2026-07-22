wit_bindgen::generate!({
    path: "../../crates/manifold-wasm/wit",
    world: "manifold-plugin",
});

use exports::manifold::plugin::actor::{AppStartRequest, Command, Event, Guest, LifecycleEvent};

struct ExampleInitActor;

impl Guest for ExampleInitActor {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(_)) => vec![
                Command::StartApp(AppStartRequest {
                    id: "terminal".to_string(),
                    argv: vec!["foot".to_string()],
                }),
                Command::LifecycleComplete(Ok(())),
            ],
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
