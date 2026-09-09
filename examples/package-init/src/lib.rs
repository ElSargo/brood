//! Minimal explicit realm root for the external-package behavior proof.
use brood_plugin_sdk::{bindings, exports::brood::plugin::actor::{Command, Event, Guest}};
struct PackageInit;
impl Guest for PackageInit {
    fn dispatch(event: Event) -> Vec<Command> {
        brood_plugin_sdk::packages::dispatch(&event).unwrap_or_else(|| match event {
            Event::Lifecycle(_) => vec![Command::LifecycleComplete(Ok(()))],
            _ => vec![],
        })
    }
    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> { Ok(vec![]) }
}
bindings::export!(PackageInit with_types_in bindings);
