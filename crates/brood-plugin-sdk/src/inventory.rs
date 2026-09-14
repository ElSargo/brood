//! Retained child inventories for reconnectable plugin presentations.
//!
//! A session supervisor feeds selected child lifecycle events to an
//! [`ActorInventory`] and forwards authenticated subscription messages to it.
//! Each subscription replies with the complete ready set, including an empty
//! set. Changes publish the same complete snapshot, so a replacement client
//! needs no history of creation notifications. Register before creating children
//! (or subscribe before they finish starting); both orderings are supported.
//!
//! The presentation subscribes whenever its session catalogue is refreshed and
//! uses [`Snapshot::changes`] against its live *and starting* children. Apply
//! removals locally before stopping presentation children, so their stop events
//! cannot accidentally stop already-removed session resources. A failed UI is
//! not a request to destroy its backing resource. Plugin policy still chooses
//! actor names, component paths, capabilities, and explicit close behavior.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    actor::{send_presentation, send_session},
    exports::brood::plugin::actor::{ChildEvent, Command},
};

const SNAPSHOT_KIND: &str = "brood.actor-inventory.v1";
const SUBSCRIBE_KIND: &str = "brood.actor-inventory-subscribe.v1";
const MAX_ACTORS: usize = 64;
const MAX_SUBSCRIBERS: usize = 16;
const MAX_MESSAGE_BYTES: usize = 8 * 1024;

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

/// Complete, bounded inventory sent by one session supervisor.
#[derive(Debug, Deserialize, Serialize)]
pub struct Snapshot {
    kind: String,
    inventory: String,
    /// Session within the authenticated sender's backend.
    pub session_id: u64,
    /// Ready session actors, in stable lexical order.
    pub actors: BTreeSet<String>,
}

impl Snapshot {
    /// Decode a snapshot for the expected inventory. The caller must also
    /// authenticate the sender and check that the session is in its catalogue.
    #[must_use]
    pub fn decode(body: &[u8], inventory: &str) -> Option<Self> {
        if body.len() > MAX_MESSAGE_BYTES {
            return None;
        }
        let snapshot: Self = serde_json::from_slice(body).ok()?;
        (snapshot.kind == SNAPSHOT_KIND
            && snapshot.inventory == inventory
            && snapshot.session_id != 0
            && snapshot.actors.len() <= MAX_ACTORS
            && snapshot.actors.iter().all(|actor| valid_name(actor)))
        .then_some(snapshot)
    }

    /// Compare with locally known resource IDs, including starts in flight.
    /// Repeated snapshots then produce no duplicate starts or stops.
    #[must_use]
    pub fn changes<'a>(&self, current: impl IntoIterator<Item = &'a str>) -> Changes {
        let current = current
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        Changes {
            added: self.actors.difference(&current).cloned().collect(),
            removed: current.difference(&self.actors).cloned().collect(),
        }
    }
}

/// Resource IDs to add and remove in the presentation.
#[derive(Debug, PartialEq, Eq)]
pub struct Changes {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct Subscription {
    kind: String,
    inventory: String,
}

/// Request and subscribe to a complete inventory. Repeating this after a
/// reconnect also repairs changes whose previous notification was not delivered.
#[must_use]
pub fn subscribe(backend: u64, session: u64, supervisor: &str, inventory: &str) -> Command {
    let body = serde_json::json!({ "kind": SUBSCRIBE_KIND, "inventory": inventory })
        .to_string()
        .into_bytes();
    send_session(backend, session, supervisor, body)
}

/// Session-owned ready children and bounded, stable presentation subscribers.
pub struct ActorInventory {
    name: &'static str,
    actors: BTreeSet<String>,
    subscribers: BTreeSet<String>,
}

impl ActorInventory {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            actors: BTreeSet::new(),
            subscribers: BTreeSet::new(),
        }
    }

    /// Admit a new child before issuing its start command. Pending children
    /// should count toward the same bound in the supervising plugin.
    #[must_use]
    pub fn has_capacity(&self, pending: usize) -> bool {
        self.actors.len().saturating_add(pending) < MAX_ACTORS
    }

    /// Observe a selected child's real readiness, stop, or failure. Failed
    /// starts never appear in the inventory. Duplicate lifecycle events do not
    /// republish it. The plugin must admit starts using [`Self::has_capacity`].
    #[must_use]
    pub fn child(&mut self, session: u64, event: &ChildEvent) -> Vec<Command> {
        let changed = match event {
            ChildEvent::Started(child) => {
                valid_name(&child.id)
                    && self.actors.len() < MAX_ACTORS
                    && self.actors.insert(child.id.clone())
            }
            ChildEvent::Stopped(id) => self.actors.remove(id),
            ChildEvent::Failed(child) => self.actors.remove(&child.id),
        };
        if !changed {
            return Vec::new();
        }
        let body = self.snapshot(session);
        self.subscribers
            .iter()
            .map(|actor| send_presentation(actor, body.clone()))
            .collect()
    }

    /// Handle an authenticated presentation subscription. Replies even for an
    /// existing subscriber: a replacement actor uses the same logical address.
    #[must_use]
    pub fn handle_subscription(
        &mut self,
        session: u64,
        sender: &str,
        body: &[u8],
    ) -> Option<Vec<Command>> {
        let actor = sender
            .strip_prefix("presentation:")
            .filter(|actor| valid_name(actor))?;
        if body.len() > MAX_MESSAGE_BYTES {
            return None;
        }
        let request: Subscription = serde_json::from_slice(body).ok()?;
        if request.kind != SUBSCRIBE_KIND || request.inventory != self.name {
            return None;
        }
        if self.subscribers.len() >= MAX_SUBSCRIBERS && !self.subscribers.contains(actor) {
            return Some(Vec::new());
        }
        self.subscribers.insert(actor.to_string());
        Some(vec![send_presentation(actor, self.snapshot(session))])
    }

    fn snapshot(&self, session: u64) -> Vec<u8> {
        serde_json::json!({
            "kind": SNAPSHOT_KIND,
            "inventory": self.name,
            "session_id": session,
            "actors": self.actors,
        })
        .to_string()
        .into_bytes()
    }
}

#[cfg(test)]
#[allow(
    clippy::panic_in_result_fn,
    reason = "tests use assertions and propagate fixture errors"
)]
mod tests {
    use super::*;
    use crate::exports::brood::plugin::actor::{ActorFailed, ActorStarted};

    fn started(id: &str) -> ChildEvent {
        ChildEvent::Started(ActorStarted {
            id: id.to_string(),
            generation: 1,
        })
    }

    fn subscribe_to(inventory: &mut ActorInventory) -> Result<Snapshot, String> {
        let Command::SendSessionMessage(request) = subscribe(2, 7, "init", "documents") else {
            return Err("unexpected command".to_string());
        };
        let commands = inventory
            .handle_subscription(7, "presentation:client-init", &request.body)
            .ok_or("missing subscription")?;
        let Command::SendPresentationMessage(reply) = commands.first().ok_or("missing reply")?
        else {
            return Err("unexpected command".to_string());
        };
        Snapshot::decode(&reply.body, "documents").ok_or_else(|| "missing snapshot".to_string())
    }

    #[test]
    fn replacement_receives_all_ready_children_and_empty_inventory() -> Result<(), String> {
        let mut inventory = ActorInventory::new("documents");
        assert!(inventory.child(7, &started("doc-1")).is_empty());
        assert!(inventory.child(7, &started("doc-2")).is_empty());
        let first = subscribe_to(&mut inventory)?;
        assert_eq!(first.actors.len(), 2);
        assert!(inventory.child(7, &started("doc-1")).is_empty());
        assert_eq!(inventory.child(7, &started("doc-3")).len(), 1);
        assert_eq!(
            inventory
                .child(7, &ChildEvent::Stopped("doc-2".to_string()))
                .len(),
            1
        );
        assert!(
            inventory
                .child(
                    7,
                    &ChildEvent::Failed(ActorFailed {
                        id: "never-ready".to_string(),
                        message: "failed".to_string()
                    })
                )
                .is_empty()
        );
        let replacement = subscribe_to(&mut inventory)?;
        assert_eq!(
            replacement.actors,
            BTreeSet::from(["doc-1".to_string(), "doc-3".to_string()])
        );
        assert_eq!(
            replacement.changes(["doc-1", "doc-2"]),
            Changes {
                added: vec!["doc-3".to_string()],
                removed: vec!["doc-2".to_string()]
            }
        );
        assert_eq!(
            replacement.changes(["doc-1", "doc-3"]),
            Changes {
                added: vec![],
                removed: vec![]
            }
        );
        assert_eq!(
            inventory
                .child(7, &ChildEvent::Stopped("doc-1".to_string()))
                .len(),
            1
        );
        assert_eq!(
            inventory
                .child(7, &ChildEvent::Stopped("doc-3".to_string()))
                .len(),
            1
        );
        assert!(subscribe_to(&mut inventory)?.actors.is_empty());
        Ok(())
    }

    #[test]
    fn subscription_before_readiness_gets_later_children() -> Result<(), String> {
        let mut inventory = ActorInventory::new("documents");
        assert!(subscribe_to(&mut inventory)?.actors.is_empty());
        assert_eq!(inventory.child(7, &started("doc-1")).len(), 1);
        assert_eq!(subscribe_to(&mut inventory)?.actors.len(), 1);
        Ok(())
    }
}
