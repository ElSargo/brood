//! Reusable opt-in package supervision policy for realm init actors.
use std::sync::Mutex;

use crate::exports::brood::plugin::actor::{
    ActorStartRequest, ChildEvent, Command, Event, PresentationScope,
};
use brood_package_contract::{Action, Child, HOST_SENDER, InitPolicy, RealmUpdate};

static POLICY: Mutex<Option<InitPolicy>> = Mutex::new(None);

/// Consume host package declarations and managed child events inside an init
/// actor. Ordinary plugin dispatchers must not opt into this root policy.
#[must_use]
pub fn dispatch(event: &Event) -> Option<Vec<Command>> {
    let (actions, report) = {
        let mut guard = POLICY.lock().ok()?;
        let policy = guard.get_or_insert_with(InitPolicy::default);
        let actions = match event {
            Event::Mailbox(message) if message.sender == HOST_SENDER => {
                let Ok(update) = RealmUpdate::decode(&message.body) else {
                    return Some(Vec::new());
                };
                policy.update(update)
            }
            Event::Child(ChildEvent::Started(child)) if policy.owns(&child.id) => {
                policy.child(Child::Started(child.id.clone()))
            }
            Event::Child(ChildEvent::Stopped(id)) if policy.owns(id) => {
                policy.child(Child::Stopped(id.clone()))
            }
            Event::Child(ChildEvent::Failed(child)) if policy.owns(&child.id) => {
                policy.child(Child::Failed(child.id.clone()))
            }
            _ => return None,
        };
        let report = policy.report().encode().ok();
        drop(guard);
        (actions, report)
    };
    Some(
        actions
            .into_iter()
            .map(|action| match action {
                Action::Start(actor) => Command::StartActor(ActorStartRequest {
                    path: actor.reference(),
                    id: actor.id,
                    capabilities: actor.capabilities,
                    presentation_scope: PresentationScope::Global,
                }),
                Action::Stop(id) => Command::StopActor(id),
            })
            .chain(report.map(Command::SendHostMessage))
            .collect(),
    )
}
