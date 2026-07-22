wit_bindgen::generate!({
    path: "../../crates/manifold-wasm/wit",
    world: "manifold-plugin",
});

use exports::manifold::plugin::actor::{Command, Event, Guest};

struct ExampleChildActor;

impl Guest for ExampleChildActor {
    fn dispatch(_event: Event) -> Vec<Command> {
        Vec::new()
    }

    async fn worker_run(_worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(payload)
    }
}

export!(ExampleChildActor with_types_in self);
