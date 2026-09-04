wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use exports::brood::plugin::actor::{Command, Event, Guest};

struct ExampleChildActor;

impl Guest for ExampleChildActor {
    fn dispatch(_event: Event) -> Vec<Command> {
        Vec::new()
    }

    async fn worker_run(_worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(payload)
    }

    fn render(
        _request: exports::brood::plugin::actor::PureRenderRequest,
    ) -> Result<
        exports::brood::plugin::actor::PureRenderResult,
        exports::brood::plugin::actor::PureRenderError,
    > {
        Err(exports::brood::plugin::actor::PureRenderError::Unsupported)
    }
}

export!(ExampleChildActor with_types_in self);
