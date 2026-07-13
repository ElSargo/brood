use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::mpsc,
};

use anyhow::{Result, anyhow};

use super::session_bus::{DBUS_DESTINATION, NOTIFICATIONS_DESTINATION, NOTIFICATIONS_UNIQUE_NAME};

const NAME_FLAG_ALLOW_REPLACEMENT: u32 = 0x1;
const NAME_FLAG_REPLACE_EXISTING: u32 = 0x2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConnectionId(u64);

impl ConnectionId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueName(String);

impl UniqueName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameOwnerChange {
    pub name: String,
    pub old_owner: String,
    pub new_owner: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestNameOutcome {
    PrimaryOwner,
    Exists,
    AlreadyOwner,
}

impl RequestNameOutcome {
    pub const fn reply_code(self) -> u32 {
        match self {
            Self::PrimaryOwner => 1,
            Self::Exists => 3,
            Self::AlreadyOwner => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseNameOutcome {
    Released,
    NonExistent,
    NotOwner,
}

impl ReleaseNameOutcome {
    pub const fn reply_code(self) -> u32 {
        match self {
            Self::Released => 1,
            Self::NonExistent => 2,
            Self::NotOwner => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameTransition<T> {
    pub outcome: T,
    pub owner_change: Option<NameOwnerChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClass {
    MethodCall,
    MethodReturn,
    Error,
    Signal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    Unicast(ConnectionId),
    Broadcast(Vec<ConnectionId>),
    UnknownDestination,
    Drop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisconnectTransition {
    pub owner_changes: Vec<NameOwnerChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationProxyTransition<T> {
    Succeeded(T),
    Failed(String),
}

impl<T> NotificationProxyTransition<T> {
    pub fn from_result(result: Result<T>) -> Self {
        match result {
            Ok(reply) => Self::Succeeded(reply),
            Err(error) => Self::Failed(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationSignal {
    ActionInvoked { id: u32, action_key: String },
    ActivationToken { id: u32, token: String },
    Closed { id: u32, reason: u32 },
}

impl NotificationSignal {
    pub const fn notification_id(&self) -> u32 {
        match self {
            Self::ActionInvoked { id, .. }
            | Self::ActivationToken { id, .. }
            | Self::Closed { id, .. } => *id,
        }
    }

    pub const fn closes_forwarder(&self) -> bool {
        matches!(self, Self::Closed { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationForwardDecision {
    Deliver(NotificationSignal),
    Ignore,
}

pub fn route_notification_signal(
    expected_id: u32,
    signal: NotificationSignal,
) -> NotificationForwardDecision {
    if signal.notification_id() == expected_id {
        NotificationForwardDecision::Deliver(signal)
    } else {
        NotificationForwardDecision::Ignore
    }
}

#[derive(Debug)]
struct ConnectionState {
    unique_name: Option<UniqueName>,
    owned_names: BTreeSet<String>,
    tx: mpsc::Sender<Vec<u8>>,
}

#[derive(Debug, Clone, Copy)]
struct NameOwner {
    connection: ConnectionId,
    allow_replacement: bool,
}

#[derive(Debug)]
pub struct BusRouter {
    next_connection: u64,
    connections: BTreeMap<ConnectionId, ConnectionState>,
    names: BTreeMap<String, NameOwner>,
    notifications_available: bool,
}

impl BusRouter {
    pub const fn new(notifications_available: bool) -> Self {
        Self {
            next_connection: 1,
            connections: BTreeMap::new(),
            names: BTreeMap::new(),
            notifications_available,
        }
    }

    pub fn connect(&mut self, tx: mpsc::Sender<Vec<u8>>) -> Result<ConnectionId> {
        let next = self
            .next_connection
            .checked_add(1)
            .ok_or_else(|| anyhow!("session bus connection id exhausted"))?;
        let id = ConnectionId(self.next_connection);
        if self.connections.contains_key(&id) {
            return Err(anyhow!("session bus connection id {id} is already live"));
        }
        self.connections.insert(
            id,
            ConnectionState {
                unique_name: None,
                owned_names: BTreeSet::new(),
                tx,
            },
        );
        self.next_connection = next;
        Ok(id)
    }

    pub fn ensure_unique(&mut self, connection: ConnectionId) -> Result<UniqueName> {
        let current = self
            .connections
            .get(&connection)
            .ok_or_else(|| anyhow!("unknown session bus connection {connection}"))?
            .unique_name
            .clone();
        if let Some(name) = current {
            return Ok(name);
        }

        let name = UniqueName(format!(":1.{}", connection.get()));
        if self.names.contains_key(name.as_str()) {
            return Err(anyhow!(
                "session bus unique name {} is already owned",
                name.as_str()
            ));
        }
        self.names.insert(
            name.0.clone(),
            NameOwner {
                connection,
                allow_replacement: false,
            },
        );
        self.connections
            .get_mut(&connection)
            .ok_or_else(|| anyhow!("unknown session bus connection {connection}"))?
            .unique_name = Some(name.clone());
        Ok(name)
    }

    pub fn request_name(
        &mut self,
        connection: ConnectionId,
        name: String,
        flags: u32,
    ) -> Result<NameTransition<RequestNameOutcome>> {
        if !valid_well_known_name(&name) {
            return Err(anyhow!("invalid well-known bus name {name}"));
        }
        let requester = self.ensure_unique(connection)?;
        let allow_replacement = flags & NAME_FLAG_ALLOW_REPLACEMENT != 0;
        let replace_existing = flags & NAME_FLAG_REPLACE_EXISTING != 0;

        let existing = self.names.get(&name).copied();
        match existing {
            Some(owner) if owner.connection == connection => {
                self.names.insert(
                    name,
                    NameOwner {
                        connection,
                        allow_replacement,
                    },
                );
                Ok(NameTransition {
                    outcome: RequestNameOutcome::AlreadyOwner,
                    owner_change: None,
                })
            }
            Some(owner) if !(replace_existing && owner.allow_replacement) => Ok(NameTransition {
                outcome: RequestNameOutcome::Exists,
                owner_change: None,
            }),
            Some(owner) => {
                let old_owner = self.unique_name_for(owner.connection)?;
                self.connections
                    .get_mut(&owner.connection)
                    .ok_or_else(|| anyhow!("name owner disappeared during replacement"))?
                    .owned_names
                    .remove(&name);
                self.connections
                    .get_mut(&connection)
                    .ok_or_else(|| anyhow!("requesting connection disappeared"))?
                    .owned_names
                    .insert(name.clone());
                self.names.insert(
                    name.clone(),
                    NameOwner {
                        connection,
                        allow_replacement,
                    },
                );
                Ok(NameTransition {
                    outcome: RequestNameOutcome::PrimaryOwner,
                    owner_change: Some(NameOwnerChange {
                        name,
                        old_owner: old_owner.0,
                        new_owner: requester.0,
                    }),
                })
            }
            None => {
                self.connections
                    .get_mut(&connection)
                    .ok_or_else(|| anyhow!("requesting connection disappeared"))?
                    .owned_names
                    .insert(name.clone());
                self.names.insert(
                    name.clone(),
                    NameOwner {
                        connection,
                        allow_replacement,
                    },
                );
                Ok(NameTransition {
                    outcome: RequestNameOutcome::PrimaryOwner,
                    owner_change: Some(NameOwnerChange {
                        name,
                        old_owner: String::new(),
                        new_owner: requester.0,
                    }),
                })
            }
        }
    }

    pub fn release_name(
        &mut self,
        connection: ConnectionId,
        name: &str,
    ) -> Result<NameTransition<ReleaseNameOutcome>> {
        if !self.connections.contains_key(&connection) {
            return Err(anyhow!("unknown session bus connection {connection}"));
        }
        let Some(owner) = self.names.get(name).copied() else {
            return Ok(NameTransition {
                outcome: ReleaseNameOutcome::NonExistent,
                owner_change: None,
            });
        };
        if owner.connection != connection {
            return Ok(NameTransition {
                outcome: ReleaseNameOutcome::NotOwner,
                owner_change: None,
            });
        }
        if name.starts_with(':') {
            return Ok(NameTransition {
                outcome: ReleaseNameOutcome::NotOwner,
                owner_change: None,
            });
        }

        let old_owner = self.unique_name_for(connection)?;
        self.names.remove(name);
        self.connections
            .get_mut(&connection)
            .ok_or_else(|| anyhow!("releasing connection disappeared"))?
            .owned_names
            .remove(name);
        Ok(NameTransition {
            outcome: ReleaseNameOutcome::Released,
            owner_change: Some(NameOwnerChange {
                name: name.to_string(),
                old_owner: old_owner.0,
                new_owner: String::new(),
            }),
        })
    }

    pub fn disconnect(&mut self, connection: ConnectionId) -> Result<DisconnectTransition> {
        let state = self
            .connections
            .remove(&connection)
            .ok_or_else(|| anyhow!("unknown session bus connection {connection}"))?;
        let old_owner = state
            .unique_name
            .as_ref()
            .map_or_else(String::new, |name| name.0.clone());
        let mut owner_changes = Vec::new();
        for name in state.owned_names {
            self.names.remove(&name);
            owner_changes.push(NameOwnerChange {
                name,
                old_owner: old_owner.clone(),
                new_owner: String::new(),
            });
        }
        if let Some(unique_name) = state.unique_name {
            self.names.remove(unique_name.as_str());
            owner_changes.push(NameOwnerChange {
                name: unique_name.0.clone(),
                old_owner: unique_name.0,
                new_owner: String::new(),
            });
        }
        Ok(DisconnectTransition { owner_changes })
    }

    pub fn name_owner(&self, name: &str) -> Option<ConnectionId> {
        if name == DBUS_DESTINATION
            || (self.notifications_available
                && matches!(name, NOTIFICATIONS_DESTINATION | NOTIFICATIONS_UNIQUE_NAME))
        {
            return Some(ConnectionId(0));
        }
        self.names.get(name).map(|owner| owner.connection)
    }

    pub fn owner_unique_name(&self, name: &str) -> Option<String> {
        let owner = self.name_owner(name)?;
        if owner.get() == 0 {
            return match name {
                DBUS_DESTINATION => Some(":1.0".to_string()),
                NOTIFICATIONS_DESTINATION | NOTIFICATIONS_UNIQUE_NAME => {
                    Some(NOTIFICATIONS_UNIQUE_NAME.to_string())
                }
                _ => None,
            };
        }
        self.unique_name_for(owner).ok().map(|name| name.0)
    }

    pub fn list_names(&self) -> Vec<String> {
        let mut names = vec![DBUS_DESTINATION.to_string()];
        if self.notifications_available {
            names.push(NOTIFICATIONS_DESTINATION.to_string());
            names.push(NOTIFICATIONS_UNIQUE_NAME.to_string());
        }
        names.extend(self.names.keys().cloned());
        names
    }

    pub fn route(
        &self,
        sender: ConnectionId,
        class: MessageClass,
        destination: Option<&str>,
    ) -> Result<RouteDecision> {
        if !self.connections.contains_key(&sender) {
            return Err(anyhow!("unknown session bus connection {sender}"));
        }
        if let Some(destination) = destination {
            return Ok(match self.name_owner(destination) {
                Some(owner) if owner != sender && owner.get() != 0 => RouteDecision::Unicast(owner),
                Some(_) if matches!(class, MessageClass::Signal) => RouteDecision::Drop,
                Some(_) => RouteDecision::UnknownDestination,
                None if matches!(class, MessageClass::MethodCall) => {
                    RouteDecision::UnknownDestination
                }
                None => RouteDecision::Drop,
            });
        }
        Ok(match class {
            MessageClass::Signal => RouteDecision::Broadcast(
                self.connections
                    .keys()
                    .copied()
                    .filter(|connection| *connection != sender)
                    .collect(),
            ),
            MessageClass::MethodCall => RouteDecision::UnknownDestination,
            MessageClass::MethodReturn | MessageClass::Error => RouteDecision::Drop,
        })
    }

    pub fn deliver(&self, recipient: ConnectionId, bytes: Vec<u8>) -> Result<()> {
        let connection = self
            .connections
            .get(&recipient)
            .ok_or_else(|| anyhow!("delivery recipient {recipient} is not connected"))?;
        connection
            .tx
            .send(bytes)
            .map_err(|error| anyhow!("deliver to session bus connection {recipient}: {error}"))
    }

    pub fn broadcast(&self, bytes: &[u8], except: Option<ConnectionId>) -> Result<()> {
        let mut failures = Vec::new();
        for (&connection, state) in &self.connections {
            if Some(connection) != except
                && let Err(error) = state.tx.send(bytes.to_owned())
            {
                failures.push(format!("{connection}: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "session bus broadcast delivery failed: {}",
                failures.join(", ")
            ))
        }
    }

    fn unique_name_for(&self, connection: ConnectionId) -> Result<UniqueName> {
        self.connections
            .get(&connection)
            .and_then(|state| state.unique_name.clone())
            .ok_or_else(|| anyhow!("connection {connection} has no unique name"))
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

fn valid_well_known_name(name: &str) -> bool {
    !name.starts_with(':') && name.contains('.') && !name.is_empty()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "transition fixtures use deterministic in-memory channels and fixed ownership states"
)]
mod tests {
    use super::*;

    fn connected_router() -> (BusRouter, ConnectionId, mpsc::Receiver<Vec<u8>>) {
        let mut router = BusRouter::new(false);
        let (tx, rx) = mpsc::channel();
        let connection = router.connect(tx).expect("connect fixture");
        (router, connection, rx)
    }

    #[test]
    fn unique_and_well_known_names_acquire_release_and_replace() {
        let (mut router, first, _first_rx) = connected_router();
        let (second_tx, _second_rx) = mpsc::channel();
        let second = router.connect(second_tx).expect("connect second");
        assert_eq!(
            router.ensure_unique(first).expect("first unique").as_str(),
            ":1.1"
        );
        assert_eq!(
            router
                .ensure_unique(second)
                .expect("second unique")
                .as_str(),
            ":1.2"
        );

        let acquired = router
            .request_name(
                first,
                "org.manifold.Typed".into(),
                NAME_FLAG_ALLOW_REPLACEMENT,
            )
            .expect("acquire name");
        assert_eq!(acquired.outcome, RequestNameOutcome::PrimaryOwner);
        assert_eq!(
            acquired
                .owner_change
                .expect("acquire owner change")
                .new_owner,
            ":1.1"
        );

        let rejected = router
            .request_name(second, "org.manifold.Typed".into(), 0)
            .expect("reject without queue");
        assert_eq!(rejected.outcome, RequestNameOutcome::Exists);
        assert_eq!(router.name_owner("org.manifold.Typed"), Some(first));

        let replaced = router
            .request_name(
                second,
                "org.manifold.Typed".into(),
                NAME_FLAG_REPLACE_EXISTING,
            )
            .expect("replace name");
        assert_eq!(replaced.outcome, RequestNameOutcome::PrimaryOwner);
        let change = replaced.owner_change.expect("replacement change");
        assert_eq!(
            (change.old_owner.as_str(), change.new_owner.as_str()),
            (":1.1", ":1.2")
        );
        assert_eq!(router.name_owner("org.manifold.Typed"), Some(second));

        let released = router
            .release_name(second, "org.manifold.Typed")
            .expect("release name");
        assert_eq!(released.outcome, ReleaseNameOutcome::Released);
        assert_eq!(router.name_owner("org.manifold.Typed"), None);
    }

    #[test]
    fn disconnect_removes_unique_and_well_known_owners() {
        let (mut router, connection, _rx) = connected_router();
        router
            .request_name(connection, "org.manifold.Disconnect".into(), 0)
            .expect("request name");
        let transition = router.disconnect(connection).expect("disconnect");
        assert_eq!(transition.owner_changes.len(), 2);
        assert!(
            transition
                .owner_changes
                .iter()
                .any(|change| change.name == ":1.1")
        );
        assert!(
            transition
                .owner_changes
                .iter()
                .any(|change| change.name == "org.manifold.Disconnect")
        );
        assert_eq!(router.name_owner("org.manifold.Disconnect"), None);
        assert_eq!(router.name_owner(":1.1"), None);
    }

    #[test]
    fn routing_distinguishes_unknown_calls_replies_and_signal_broadcasts() {
        let (mut router, first, _first_rx) = connected_router();
        let (second_tx, _second_rx) = mpsc::channel();
        let second = router.connect(second_tx).expect("connect second");
        router.ensure_unique(second).expect("second unique");

        assert_eq!(
            router
                .route(first, MessageClass::MethodCall, Some("org.unknown.Service"))
                .expect("route"),
            RouteDecision::UnknownDestination
        );
        assert_eq!(
            router
                .route(
                    first,
                    MessageClass::MethodReturn,
                    Some("org.unknown.Service"),
                )
                .expect("route"),
            RouteDecision::Drop
        );
        assert_eq!(
            router
                .route(first, MessageClass::MethodReturn, Some(":1.2"))
                .expect("route"),
            RouteDecision::Unicast(second)
        );
        assert_eq!(
            router
                .route(first, MessageClass::Error, Some(":1.2"))
                .expect("route"),
            RouteDecision::Unicast(second)
        );
        assert_eq!(
            router
                .route(first, MessageClass::Signal, None)
                .expect("route"),
            RouteDecision::Broadcast(vec![second])
        );
        assert_eq!(
            router
                .route(first, MessageClass::Signal, Some("org.unknown.Service"))
                .expect("route"),
            RouteDecision::Drop
        );
    }

    #[test]
    fn notification_proxy_and_forwarding_transitions_are_explicit() {
        assert_eq!(
            NotificationProxyTransition::from_result(Ok(17_u32)),
            NotificationProxyTransition::Succeeded(17)
        );
        let failed =
            NotificationProxyTransition::<u32>::from_result(Err(anyhow!("host unavailable")));
        assert_eq!(
            failed,
            NotificationProxyTransition::Failed("host unavailable".into())
        );
        let signal = NotificationSignal::ActivationToken {
            id: 17,
            token: "focus-me".into(),
        };
        assert_eq!(
            route_notification_signal(17, signal.clone()),
            NotificationForwardDecision::Deliver(signal)
        );
        assert_eq!(
            route_notification_signal(18, NotificationSignal::Closed { id: 17, reason: 2 }),
            NotificationForwardDecision::Ignore
        );
    }

    #[test]
    fn built_in_owner_identities_are_explicit_for_get_name_owner() {
        let router = BusRouter::new(true);
        assert_eq!(
            router.owner_unique_name(DBUS_DESTINATION).as_deref(),
            Some(":1.0")
        );
        assert_eq!(
            router
                .owner_unique_name(NOTIFICATIONS_DESTINATION)
                .as_deref(),
            Some(NOTIFICATIONS_UNIQUE_NAME)
        );
        assert_eq!(
            router
                .owner_unique_name(NOTIFICATIONS_UNIQUE_NAME)
                .as_deref(),
            Some(NOTIFICATIONS_UNIQUE_NAME)
        );
    }

    #[test]
    fn send_failure_is_reported_at_delivery_boundary() {
        let (router, connection, rx) = connected_router();
        drop(rx);
        let error = router
            .deliver(connection, b"visible payload".to_vec())
            .expect_err("closed receiver");
        assert!(
            error
                .to_string()
                .contains("deliver to session bus connection 1")
        );
    }

    #[test]
    fn separate_router_realms_are_isolated() {
        let (mut first, connection, _rx) = connected_router();
        first
            .request_name(connection, "org.manifold.Isolated".into(), 0)
            .expect("first realm name");
        let second = BusRouter::new(false);
        assert_eq!(second.name_owner("org.manifold.Isolated"), None);
    }

    #[test]
    fn rejected_name_transition_does_not_partially_mutate_ownership() {
        let (mut router, connection, _rx) = connected_router();
        router.ensure_unique(connection).expect("unique name");
        let names_before = router.list_names();
        let error = router
            .request_name(
                connection,
                ":not.well.known".into(),
                NAME_FLAG_REPLACE_EXISTING,
            )
            .expect_err("invalid name");
        assert!(error.to_string().contains("invalid well-known"));
        assert_eq!(router.list_names(), names_before);
    }
}
