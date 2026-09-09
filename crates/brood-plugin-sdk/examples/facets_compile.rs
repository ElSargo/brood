use brood_plugin_sdk::exports::brood::plugin::actor::{
    Command, Event, Guest as ActorGuest, WorkerId,
};
use brood_plugin_sdk::{Facets, export_visual_plugin};

struct VisualPlugin;

impl ActorGuest for VisualPlugin {
    fn dispatch(_event: Event) -> Vec<Command> {
        Vec::new()
    }

    async fn worker_run(_worker: WorkerId, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(payload)
    }
}

impl Facets for VisualPlugin {
    fn render(input: Vec<u8>) -> Vec<u8> {
        input
    }

    fn interpret(input: Vec<u8>) -> Vec<u8> {
        input
    }
}

export_visual_plugin!(VisualPlugin);
