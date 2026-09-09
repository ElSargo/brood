//! Host-owned service bindings. Guest messages cannot supply peer identities.
use crate::package_runtime::{
    InstanceRecord,
    contract::{
        self, Realm,
        services::{
            Binding, Event, MAX_INTERFACES, MAX_PAYLOAD_BYTES, MAX_WIRE_BYTES, Packet, Peer,
            Registration, Role,
        },
    },
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_QUEUE: usize = 1024;
const MAX_BINDINGS: usize = 512;
const OBSERVATION_LEASE: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone, Debug)]
pub enum Delivery {
    Guest {
        actor: String,
        generation: u64,
        event: Event,
    },
    Remote {
        route: String,
        packet: Packet,
    },
}

impl Delivery {
    #[must_use]
    pub fn lane(&self) -> String {
        match self {
            Self::Guest {
                actor, generation, ..
            } => format!("guest:{actor}:{generation}"),
            Self::Remote { route, .. } => format!("remote:{route}"),
        }
    }
}

#[derive(Clone)]
struct Observation {
    peer: Peer,
    facts: serde_json::Value,
    route: Option<String>,
}
struct Route {
    scope: Option<String>,
    last_seen: std::time::Instant,
    connected: bool,
}

/// One broker per host process. Routes name authenticated, generation-scoped
/// transport connections supplied by the embedding host, never by a guest.
pub struct Broker {
    endpoint: String,
    peers: BTreeMap<String, Observation>,
    routes: BTreeMap<String, Route>,
    bindings: BTreeMap<String, Binding>,
    queue: VecDeque<Delivery>,
    lost_services: BTreeMap<(Realm, String, String, String), (Observation, contract::Interface)>,
    revision: u64,
    validated_messages: u64,
    local_records: Vec<InstanceRecord>,
    remote_records: BTreeMap<(String, String), serde_json::Value>,
}

impl Default for Broker {
    fn default() -> Self {
        Self {
            endpoint: format!("host:{}:{}", std::process::id(), unix_ms()),
            peers: BTreeMap::new(),
            routes: BTreeMap::new(),
            bindings: BTreeMap::new(),
            queue: VecDeque::new(),
            lost_services: BTreeMap::new(),
            revision: 0,
            validated_messages: 0,
            local_records: vec![],
            remote_records: BTreeMap::new(),
        }
    }
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_millis())
}
fn supports(peer: &Peer, interface: &contract::Interface) -> bool {
    peer.registered
        .get(&interface.id)
        .is_some_and(|versions| versions.contains(&interface.version))
}
fn compatible(a: &contract::Interface, b: &contract::Interface) -> bool {
    a.id == b.id && a.version == b.version && a.schema_digest == b.schema_digest
}
fn binding_id(consumer: &Peer, provider: &Peer, interface: &contract::Interface) -> Result<String> {
    Ok(brood_package_manifest::installation::digest(
        &serde_json::to_vec(&(consumer.key(), provider.key(), interface))?,
    ))
}

impl Broker {
    fn enqueue(&mut self, delivery: Delivery) -> Result<()> {
        ensure!(
            self.queue.len() < MAX_QUEUE,
            "service delivery queue is full"
        );
        match &delivery {
            Delivery::Remote { packet, .. } => {
                packet.encode().map_err(anyhow::Error::msg)?;
            }
            Delivery::Guest { event, .. } => {
                ensure!(
                    serde_json::to_vec(event)?.len() <= MAX_WIRE_BYTES,
                    "service event exceeds mailbox limit"
                );
            }
        }
        self.queue.push_back(delivery);
        Ok(())
    }
    fn guest(&mut self, peer: &Peer, event: Event) -> Result<()> {
        self.enqueue(Delivery::Guest {
            actor: peer.actor.clone(),
            generation: peer.generation,
            event,
        })
    }
    fn remote(&mut self, route: &str, packet: Packet) -> Result<()> {
        self.enqueue(Delivery::Remote {
            route: route.into(),
            packet,
        })
    }
    pub fn pop(&mut self) -> Option<Delivery> {
        self.queue.pop_front()
    }
    pub fn pop_unblocked(&mut self, blocked: &BTreeSet<String>) -> Option<Delivery> {
        let index = self
            .queue
            .iter()
            .position(|delivery| !blocked.contains(&delivery.lane()))?;
        self.queue.remove(index)
    }
    pub fn defer_lanes(&mut self, blocked: &BTreeSet<String>) {
        let mut deferred = VecDeque::new();
        for delivery in std::mem::take(&mut self.queue) {
            if blocked.contains(&delivery.lane()) {
                deferred.push_back(delivery);
            } else {
                self.queue.push_back(delivery);
            }
        }
        self.queue.extend(deferred);
    }
    pub fn retry(&mut self, delivery: Delivery) {
        self.queue.push_front(delivery);
    }

    /// Sync only host facts, preserving registration solely for the exact actor generation.
    pub fn sync(&mut self, records: &[InstanceRecord]) {
        self.local_records = records.iter().take(contract::MAX_ACTORS).cloned().collect();
        let mut live = BTreeSet::new();
        for record in records
            .iter()
            .filter(|record| record.lifecycle == "running")
            .take(contract::MAX_ACTORS)
        {
            let Some(generation) = record.generation else {
                continue;
            };
            let mut peer = Peer {
                endpoint: self.endpoint.clone(),
                realm: record.realm,
                scope: record.scope.clone(),
                actor: record.actor.clone(),
                generation,
                identity: record.declaration.identity.clone(),
                provides: record.declaration.provides.clone(),
                requires: record.declaration.requires.clone(),
                registered: BTreeMap::new(),
            };
            let key = peer.key();
            if let Some(previous) = self.peers.get(&key) {
                peer.registered.clone_from(&previous.peer.registered);
            }
            live.insert(key.clone());
            self.peers.insert(
                key,
                Observation {
                    peer,
                    facts: serde_json::to_value(record).unwrap_or_default(),
                    route: None,
                },
            );
        }
        let removed: Vec<_> = self
            .peers
            .iter()
            .filter(|(key, observation)| observation.route.is_none() && !live.contains(*key))
            .map(|(key, _)| key.clone())
            .collect();
        for key in removed {
            self.remove_peer(&key, "actor generation retired");
        }
    }
    pub fn connect(&mut self, route: &str, scope: Option<String>) {
        if !self.routes.contains_key(route) && self.routes.len() >= 32 {
            if let Some(stale) = self
                .routes
                .iter()
                .filter(|(_, state)| !state.connected)
                .min_by_key(|(_, state)| state.last_seen)
                .map(|(id, _)| id.clone())
            {
                self.routes.remove(&stale);
                self.remote_records
                    .retain(|(source, _), _| source != &stale);
            } else {
                return;
            }
        }

        if self
            .routes
            .get(route)
            .is_some_and(|previous| previous.scope != scope)
        {
            self.disconnect(route);
        }
        self.routes
            .entry(route.into())
            .and_modify(|entry| {
                entry.connected = true;
                entry.scope.clone_from(&scope);
            })
            .or_insert_with(|| Route {
                scope,
                connected: true,
                last_seen: std::time::Instant::now(),
            });
    }
    pub fn retain_routes(&mut self, live: &[String]) {
        let stale: Vec<_> = self
            .routes
            .iter()
            .filter(|(id, route)| route.connected && !live.contains(id))
            .map(|(id, _)| id.clone())
            .collect();
        for route in stale {
            self.disconnect(&route);
        }
    }
    pub fn disconnect(&mut self, route: &str) {
        if let Some(state) = self.routes.get_mut(route) {
            state.connected = false;
        }
        let keys: Vec<_> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.route.as_deref() == Some(route))
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.remove_peer(&key, "endpoint disconnected");
        }
        self.queue.retain(|delivery| !matches!(delivery, Delivery::Remote { route: queued, .. } if queued == route));
    }
    fn remove_peer(&mut self, key: &str, reason: &str) {
        let ids: Vec<_> = self
            .bindings
            .iter()
            .filter(|(_, binding)| binding.consumer.key() == key || binding.provider.key() == key)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.revoke(&id, reason);
        }
        self.peers.remove(key);
    }
    fn revoke(&mut self, id: &str, reason: &str) {
        if let Some(binding) = self.bindings.remove(id) {
            if binding.status == "ready"
                && binding.interface.disconnect == "stop"
                && let Some(consumer) = self
                    .peers
                    .get(&binding.consumer.key())
                    .filter(|peer| peer.route.is_none())
                && self.lost_services.len() < MAX_BINDINGS
            {
                self.lost_services.insert(
                    (
                        consumer.peer.realm,
                        consumer.peer.scope.clone(),
                        contract::actor_id(
                            &consumer.peer.identity.package,
                            &consumer.peer.identity.artifact,
                        ),
                        binding.interface.id.clone(),
                    ),
                    (consumer.clone(), binding.interface.clone()),
                );
            }

            for peer in [&binding.consumer, &binding.provider] {
                if let Some(observation) = self.peers.get(&peer.key()) {
                    if let Some(route) = observation.route.clone() {
                        let _notification = self.remote(
                            &route,
                            Packet::Revoke {
                                version: contract::VERSION,
                                binding: id.into(),
                                reason: reason.into(),
                            },
                        );
                    } else {
                        let _notification = self.guest(
                            peer,
                            Event::Unbound {
                                binding: id.into(),
                                reason: reason.into(),
                            },
                        );
                    }
                }
            }
        }
    }
    /// Preserve loss facts after the root retires the consumer. A replacement
    /// selection or a compatible reachable provider releases the pending policy.
    pub fn unavailable_services(
        &mut self,
        realm: Realm,
        scope: &str,
        desired: &[contract::DesiredActor],
    ) -> BTreeMap<String, Vec<String>> {
        let mut unavailable: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut resolved = Vec::new();
        for (key, (consumer, interface)) in &self.lost_services {
            if key.0 != realm || key.1 != scope {
                continue;
            }
            let selected = desired
                .iter()
                .any(|actor| actor.id == key.2 && actor.identity == consumer.peer.identity);
            let count = self
                .peers
                .values()
                .filter(|provider| {
                    self.eligible(consumer, provider)
                        && provider.peer.provides.iter().any(|provided| {
                            compatible(interface, provided) && supports(&provider.peer, provided)
                        })
                })
                .count();
            if !selected || (count > 0 && (interface.multiplicity != "one" || count == 1)) {
                resolved.push(key.clone());
            } else {
                unavailable
                    .entry(key.2.clone())
                    .or_default()
                    .push(interface.id.clone());
            }
        }
        for key in resolved {
            self.lost_services.remove(&key);
        }
        unavailable
    }

    pub fn register(
        &mut self,
        actor: &str,
        generation: u64,
        registrations: Vec<Registration>,
    ) -> Result<()> {
        ensure!(
            registrations.len() <= MAX_INTERFACES,
            "too many service registrations"
        );
        let observation = self
            .peers
            .values_mut()
            .find(|peer| {
                peer.route.is_none()
                    && peer.peer.actor == actor
                    && peer.peer.generation == generation
            })
            .context("actor generation is not hosted")?;
        let mut registered = BTreeMap::new();
        for registration in registrations {
            ensure!(
                registration.supported_versions.len() <= 16
                    && !registration.supported_versions.is_empty(),
                "invalid supported version count"
            );
            ensure!(
                observation
                    .peer
                    .provides
                    .iter()
                    .chain(&observation.peer.requires)
                    .any(|interface| interface.id == registration.interface),
                "service was not declared by the installed artifact"
            );
            ensure!(
                registered
                    .insert(registration.interface, registration.supported_versions)
                    .is_none(),
                "duplicate service registration"
            );
        }
        ensure!(
            observation.peer.registered.is_empty() || observation.peer.registered == registered,
            "service registration is immutable within an actor generation"
        );
        observation.peer.registered = registered;
        let peer = observation.peer.clone();
        self.guest(
            &peer,
            Event::Registered {
                interfaces: peer.registered.keys().cloned().collect(),
            },
        )
    }
    fn eligible(&self, consumer: &Observation, provider: &Observation) -> bool {
        if consumer.peer.key() == provider.peer.key() {
            return false;
        }
        if consumer.route.is_none() && provider.route.is_none() {
            return consumer.peer.realm == provider.peer.realm
                && consumer.peer.scope == provider.peer.scope;
        }
        let (local, remote) = if consumer.route.is_none() {
            (consumer, provider)
        } else {
            (provider, consumer)
        };
        let Some(route) = remote
            .route
            .as_ref()
            .and_then(|route| self.routes.get(route))
        else {
            return false;
        };
        route.connected
            && route.scope.as_ref().is_none_or(|scope| {
                local.peer.realm != Realm::SurfaceSession || &local.peer.scope == scope
            })
    }
    fn valid_binding(&self, binding: &Binding) -> bool {
        let Some(consumer) = self.peers.get(&binding.consumer.key()) else {
            return false;
        };
        let Some(provider) = self.peers.get(&binding.provider.key()) else {
            return false;
        };
        consumer.peer == binding.consumer
            && provider.peer == binding.provider
            && self.eligible(consumer, provider)
            && binding_id(&binding.consumer, &binding.provider, &binding.interface)
                .is_ok_and(|id| id == binding.id)
            && consumer.peer.requires.contains(&binding.interface)
            && supports(&consumer.peer, &binding.interface)
            && provider.peer.provides.iter().any(|interface| {
                compatible(interface, &binding.interface) && supports(&provider.peer, interface)
            })
    }
    pub fn reconcile(&mut self) -> Result<()> {
        let expired: Vec<_> = self
            .routes
            .iter()
            .filter(|(_, route)| route.connected && route.last_seen.elapsed() > OBSERVATION_LEASE)
            .map(|(id, _)| id.clone())
            .collect();
        for route in expired {
            self.disconnect(&route);
        }

        let stale: Vec<_> = self
            .bindings
            .iter()
            .filter(|(_, binding)| !self.valid_binding(binding))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.revoke(&id, "peer contract changed");
        }
        let consumers: Vec<_> = self
            .peers
            .values()
            .filter(|peer| peer.route.is_none())
            .cloned()
            .collect();
        let mut wanted = BTreeSet::new();
        for consumer in consumers {
            for interface in &consumer.peer.requires {
                if !supports(&consumer.peer, interface) {
                    continue;
                }
                let providers: Vec<_> = self
                    .peers
                    .values()
                    .filter(|provider| {
                        self.eligible(&consumer, provider)
                            && provider.peer.provides.iter().any(|provided| {
                                compatible(interface, provided)
                                    && supports(&provider.peer, provided)
                            })
                    })
                    .cloned()
                    .collect();
                if interface.multiplicity == "one" && providers.len() != 1 {
                    continue;
                }
                for provider in providers {
                    let id = binding_id(&consumer.peer, &provider.peer, interface)?;
                    wanted.insert(id.clone());
                    if self
                        .bindings
                        .get(&id)
                        .is_some_and(|binding| binding.status == "ready")
                    {
                        continue;
                    }
                    ensure!(
                        self.bindings.len() < MAX_BINDINGS || self.bindings.contains_key(&id),
                        "service binding limit reached"
                    );
                    let binding = Binding {
                        id: id.clone(),
                        interface: interface.clone(),
                        consumer: consumer.peer.clone(),
                        provider: provider.peer.clone(),
                        status: "offered".into(),
                    };
                    if let Some(route) = &provider.route {
                        self.remote(
                            route,
                            Packet::Offer {
                                version: contract::VERSION,
                                binding: Box::new(binding.clone()),
                            },
                        )?;
                        self.bindings.insert(id, binding);
                    } else {
                        self.notify_bound(&binding, true, true)?;
                        self.bindings.insert(
                            id,
                            Binding {
                                status: "ready".into(),
                                ..binding
                            },
                        );
                    }
                }
            }
        }
        let ambiguous: Vec<_> = self
            .bindings
            .iter()
            .filter(|(id, binding)| {
                binding.consumer.endpoint == self.endpoint && !wanted.contains(*id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ambiguous {
            self.revoke(&id, "required peer absent or ambiguous");
        }
        Ok(())
    }
    fn notify_bound(&mut self, binding: &Binding, consumer: bool, provider: bool) -> Result<()> {
        ensure!(
            self.queue.len() + usize::from(consumer) + usize::from(provider) <= MAX_QUEUE,
            "service delivery queue is full"
        );
        if consumer {
            self.guest(
                &binding.consumer,
                Event::Bound {
                    binding: binding.id.clone(),
                    role: Role::Consumer,
                    interface: binding.interface.clone(),
                    peer: Box::new(binding.provider.clone()),
                },
            )?;
        }
        if provider {
            self.guest(
                &binding.provider,
                Event::Bound {
                    binding: binding.id.clone(),
                    role: Role::Provider,
                    interface: binding.interface.clone(),
                    peer: Box::new(binding.consumer.clone()),
                },
            )?;
        }
        Ok(())
    }
    pub fn publish(&mut self) -> Result<()> {
        self.revision = self.revision.saturating_add(1);
        let routes: Vec<_> = self
            .routes
            .iter()
            .filter(|(_, route)| route.connected)
            .map(|(id, route)| (id.clone(), route.scope.clone()))
            .collect();
        for (route, scope) in routes {
            let records: Vec<_> = self
                .local_records
                .iter()
                .filter(|record| {
                    record.realm != Realm::SurfaceSession
                        || scope.as_ref().is_none_or(|scope| scope == &record.scope)
                })
                .cloned()
                .collect();
            for record in &records {
                self.remote(
                    &route,
                    Packet::Instance {
                        version: contract::VERSION,
                        key: record.actor.clone(),
                        facts: serde_json::to_value(record)?,
                    },
                )?;
            }

            let observations: Vec<_> = self
                .peers
                .values()
                .filter(|peer| {
                    peer.route.is_none()
                        && (peer.peer.realm != Realm::SurfaceSession
                            || scope.as_ref().is_none_or(|scope| scope == &peer.peer.scope))
                })
                .cloned()
                .collect();
            for peer in &observations {
                self.remote(
                    &route,
                    Packet::Observe {
                        version: contract::VERSION,
                        peer: Box::new(peer.peer.clone()),
                        facts: peer.facts.clone(),
                    },
                )?;
            }
            self.remote(
                &route,
                Packet::Heartbeat {
                    version: contract::VERSION,
                    endpoint: self.endpoint.clone(),
                    revision: self.revision,
                    keys: observations.iter().map(|peer| peer.peer.key()).collect(),
                    instance_keys: records.iter().map(|record| record.actor.clone()).collect(),
                },
            )?;
        }
        Ok(())
    }
    /// Payload validation is supplied by the immutable installed schema on this host.
    pub fn receive(
        &mut self,
        route: &str,
        packet: Packet,
        validate: impl Fn(&Peer, &contract::Interface, &serde_json::Value) -> Result<()>,
    ) -> Result<()> {
        let state = self
            .routes
            .get_mut(route)
            .context("unknown service transport")?;
        ensure!(state.connected, "service transport disconnected");
        state.last_seen = std::time::Instant::now();
        match packet {
            Packet::Instance { key, facts, .. } => {
                ensure!(
                    key.len() <= 2048 && facts.is_object(),
                    "invalid instance observation"
                );
                let key = (route.into(), key);
                ensure!(
                    self.remote_records.len() < contract::MAX_ACTORS * 4
                        || self.remote_records.contains_key(&key),
                    "instance observation limit reached"
                );
                self.remote_records.insert(key, facts);
            }
            Packet::Observe { peer, facts, .. } => {
                ensure!(
                    peer.endpoint != self.endpoint
                        && peer.provides.len() + peer.requires.len() <= MAX_INTERFACES
                        && peer.registered.len() <= MAX_INTERFACES,
                    "invalid endpoint observation"
                );
                ensure!(
                    peer.key().len() <= 2048
                        && (self.peers.len() < contract::MAX_ACTORS * 4
                            || self.peers.contains_key(&peer.key())),
                    "endpoint observation limit reached"
                );
                ensure!(
                    self.peers
                        .get(&peer.key())
                        .is_none_or(|old| old.route.as_deref() == Some(route)),
                    "peer identity belongs to another transport"
                );
                self.peers.insert(
                    peer.key(),
                    Observation {
                        peer: *peer,
                        facts,
                        route: Some(route.into()),
                    },
                );
            }
            Packet::Heartbeat {
                endpoint,
                keys,
                instance_keys,
                ..
            } => {
                ensure!(
                    keys.len() <= contract::MAX_ACTORS,
                    "heartbeat peer limit reached"
                );
                ensure!(
                    instance_keys.len() <= contract::MAX_ACTORS,
                    "heartbeat instance limit reached"
                );
                let instance_keys: BTreeSet<_> = instance_keys.into_iter().collect();
                self.remote_records
                    .retain(|(source, key), _| source != route || instance_keys.contains(key));
                let keys: BTreeSet<_> = keys.into_iter().collect();
                let removed: Vec<_> = self
                    .peers
                    .iter()
                    .filter(|(key, peer)| {
                        peer.route.as_deref() == Some(route)
                            && (peer.peer.endpoint != endpoint || !keys.contains(*key))
                    })
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in removed {
                    self.remove_peer(&key, "endpoint retired actor generation");
                }
            }
            Packet::Offer { binding, .. } => {
                ensure!(
                    binding.provider.endpoint == self.endpoint
                        && self
                            .peers
                            .get(&binding.consumer.key())
                            .is_some_and(|peer| peer.route.as_deref() == Some(route))
                        && self.valid_binding(&binding),
                    "incompatible or stale binding offer"
                );
                ensure!(
                    self.bindings.len() < MAX_BINDINGS || self.bindings.contains_key(&binding.id),
                    "service binding limit reached"
                );
                self.remote(
                    route,
                    Packet::Accept {
                        version: contract::VERSION,
                        binding: binding.id.clone(),
                    },
                )?;
                self.bindings
                    .entry(binding.id.clone())
                    .or_insert_with(|| Binding {
                        status: "accepted".into(),
                        ..*binding
                    });
            }
            Packet::Accept { binding: id, .. } => {
                let binding = self
                    .bindings
                    .get(&id)
                    .context("unknown binding acknowledgement")?
                    .clone();
                ensure!(
                    binding.consumer.endpoint == self.endpoint
                        && self
                            .peers
                            .get(&binding.provider.key())
                            .is_some_and(|peer| peer.route.as_deref() == Some(route))
                        && self.valid_binding(&binding),
                    "stale binding acknowledgement"
                );
                self.remote(
                    route,
                    Packet::Commit {
                        version: contract::VERSION,
                        binding: id.clone(),
                    },
                )?;
                if binding.status != "ready" {
                    self.notify_bound(&binding, true, false)?;
                }
                self.bindings.insert(
                    id,
                    Binding {
                        status: "ready".into(),
                        ..binding
                    },
                );
            }
            Packet::Commit { binding: id, .. } => {
                let binding = self
                    .bindings
                    .get(&id)
                    .context("unknown binding commit")?
                    .clone();
                ensure!(
                    binding.provider.endpoint == self.endpoint
                        && self
                            .peers
                            .get(&binding.consumer.key())
                            .is_some_and(|peer| peer.route.as_deref() == Some(route))
                        && self.valid_binding(&binding),
                    "stale binding commit"
                );
                if binding.status != "ready" {
                    self.notify_bound(&binding, false, true)?;
                }
                self.bindings.insert(
                    id,
                    Binding {
                        status: "ready".into(),
                        ..binding
                    },
                );
            }
            Packet::Revoke {
                binding: id,
                reason,
                ..
            } => {
                if let Some(binding) = self.bindings.get(&id) {
                    ensure!(
                        [&binding.consumer, &binding.provider].iter().any(|peer| {
                            self.peers.get(&peer.key()).is_some_and(|observation| {
                                observation.route.as_deref() == Some(route)
                            })
                        }),
                        "binding belongs to another transport"
                    );
                    self.revoke(&id, &reason);
                }
            }
            Packet::Message {
                binding: id,
                sender,
                payload,
                ..
            } => {
                let binding = self
                    .bindings
                    .get(&id)
                    .context("unknown message binding")?
                    .clone();
                ensure!(
                    binding.status == "ready" && self.valid_binding(&binding),
                    "binding is not ready"
                );
                let (source, destination) = if binding.consumer.key() == sender {
                    (&binding.consumer, &binding.provider)
                } else {
                    ensure!(
                        binding.provider.key() == sender,
                        "sender is not a bound peer"
                    );
                    (&binding.provider, &binding.consumer)
                };
                ensure!(
                    destination.endpoint == self.endpoint
                        && self
                            .peers
                            .get(&source.key())
                            .is_some_and(|peer| peer.route.as_deref() == Some(route)),
                    "message source belongs to another transport"
                );
                ensure!(
                    serde_json::to_vec(&payload)?.len() <= MAX_PAYLOAD_BYTES,
                    "service payload exceeds limit"
                );
                validate(destination, &binding.interface, &payload)?;
                self.guest(
                    destination,
                    Event::Message {
                        binding: id,
                        payload,
                    },
                )?;
                self.validated_messages = self.validated_messages.saturating_add(1);
            }
        }
        Ok(())
    }
    pub fn send(
        &mut self,
        actor: &str,
        generation: u64,
        id: &str,
        payload: serde_json::Value,
        validate: impl Fn(&Peer, &contract::Interface, &serde_json::Value) -> Result<()>,
    ) -> Result<()> {
        let binding = self
            .bindings
            .get(id)
            .context("unknown service binding")?
            .clone();
        ensure!(
            binding.status == "ready" && self.valid_binding(&binding),
            "service binding is not ready"
        );
        let owns = |peer: &Peer| {
            peer.endpoint == self.endpoint && peer.actor == actor && peer.generation == generation
        };
        let (source, destination) = if owns(&binding.consumer) {
            (&binding.consumer, &binding.provider)
        } else {
            ensure!(
                owns(&binding.provider),
                "actor generation does not own binding"
            );
            (&binding.provider, &binding.consumer)
        };
        ensure!(
            serde_json::to_vec(&payload)?.len() <= MAX_PAYLOAD_BYTES,
            "service payload exceeds limit"
        );
        validate(source, &binding.interface, &payload)?;
        self.validated_messages = self.validated_messages.saturating_add(1);
        if let Some(route) = self
            .peers
            .get(&destination.key())
            .and_then(|peer| peer.route.clone())
        {
            self.remote(
                &route,
                Packet::Message {
                    version: contract::VERSION,
                    binding: id.into(),
                    sender: source.key(),
                    payload,
                },
            )
        } else {
            validate(destination, &binding.interface, &payload)?;
            self.guest(
                destination,
                Event::Message {
                    binding: id.into(),
                    payload,
                },
            )
        }
    }
    pub fn inspection(
        &mut self,
        actor: &str,
        generation: u64,
        request_id: u64,
        response: contract::introspection::Response,
    ) -> Result<()> {
        self.enqueue(Delivery::Guest {
            actor: actor.into(),
            generation,
            event: Event::Inspection {
                request_id,
                response: Box::new(response),
            },
        })
    }

    pub fn error(&mut self, actor: &str, generation: u64, message: String) {
        let _notification = self.enqueue(Delivery::Guest {
            actor: actor.into(),
            generation,
            event: Event::Error {
                code: "service-request-rejected".into(),
                message,
            },
        });
    }
    #[must_use]
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({"endpoint": self.endpoint, "revision": self.revision, "validated_messages": self.validated_messages, "authority": "host-observed",
            "bindings": self.bindings.values().collect::<Vec<_>>(),
            "instances": self.remote_records.iter().map(|((route, key), facts)| serde_json::json!({"route": route, "key": key, "facts": facts, "authority": "endpoint-reported", "availability": if self.routes.get(route).is_some_and(|state| state.connected) {"connected"} else {"unreachable"}})).collect::<Vec<_>>(),
            "peers": self.peers.values().map(|observation| {
                let peer = &observation.peer;
                let missing: Vec<_> = peer.requires.iter().filter(|interface| !self.bindings.values().any(|binding| binding.consumer.key() == peer.key() && binding.interface.id == interface.id && binding.status == "ready")).map(|interface| &interface.id).collect();
                serde_json::json!({"peer": peer, "facts": observation.facts, "authority": if observation.route.is_some() {"endpoint-reported"} else {"host-observed"}, "readiness": if missing.is_empty() && peer.provides.iter().chain(&peer.requires).all(|interface| supports(peer, interface)) {"ready"} else {"pending"}, "missing_required_bindings": missing})
            }).collect::<Vec<_>>(),
            "endpoints": self.routes.iter().map(|(route, state)| serde_json::json!({"route": route, "status": if state.connected {"connected"} else {"unreachable"}, "observation_age_ms": state.last_seen.elapsed().as_millis()})).collect::<Vec<_>>()})
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "test assertions require concrete fixtures and binding entries"
)]
mod tests {
    use super::*;
    fn record(provider: bool, generation: u64) -> InstanceRecord {
        let interface = contract::Interface {
            id: "echo".into(),
            version: 1,
            schema_digest: "schema".into(),
            multiplicity: "one".into(),
            disconnect: "pending".into(),
        };
        InstanceRecord {
            realm: if provider {
                Realm::SurfaceSession
            } else {
                Realm::PresentationClient
            },
            scope: "1".into(),
            actor: if provider { "provider" } else { "consumer" }.into(),
            owner: "init".into(),
            declaration: contract::DesiredActor {
                id: "guest".into(),
                identity: contract::ArtifactIdentity {
                    package: "example".into(),
                    artifact: "peer".into(),
                    installation: "installed".into(),
                    artifact_digest: "wasm".into(),
                    knowledge_digest: "knowledge".into(),
                },
                capabilities: vec![],
                provides: if provider {
                    vec![interface.clone()]
                } else {
                    vec![]
                },
                requires: if provider { vec![] } else { vec![interface] },
                restart_on_failure: false,
                state_compatibility: "stateless".into(),
                migration: "none".into(),
                resource_expectations: BTreeMap::new(),
                blocked: None,
            },
            generation: Some(generation),
            lifecycle: "running".into(),
            effective_grants: vec![],
            resources: BTreeMap::new(),
            resources_observed_at_unix_ms: None,
            recent_failures: vec![],
            diagnostics: BTreeMap::new(),
            resource_assessment: BTreeMap::new(),
        }
    }
    fn broker(provider: bool, version: u32) -> Broker {
        let record = record(provider, 1);
        let mut broker = Broker {
            endpoint: record.actor.clone(),
            ..Broker::default()
        };
        broker.sync(std::slice::from_ref(&record));
        broker.connect("transport", if provider { Some("1".into()) } else { None });
        broker
            .register(
                &record.actor,
                1,
                vec![Registration {
                    interface: "echo".into(),
                    supported_versions: vec![version],
                }],
            )
            .unwrap();
        broker
    }
    fn transfer(from: &mut Broker, to: &mut Broker) -> Vec<Event> {
        let mut events = vec![];
        while let Some(delivery) = from.pop() {
            match delivery {
                Delivery::Remote { route, packet } => {
                    to.receive(&route, packet, |_, _, _| Ok(())).unwrap();
                }
                Delivery::Guest { event, .. } => events.push(event),
            }
        }
        events
    }
    fn observe(consumer: &mut Broker, provider: &mut Broker) {
        consumer.publish().unwrap();
        provider.publish().unwrap();
        transfer(consumer, provider);
        transfer(provider, consumer);
    }
    #[test]
    fn binding_requires_both_hosts_and_rejects_retired_generations() {
        let mut consumer = broker(false, 1);
        let mut provider = broker(true, 1);
        observe(&mut consumer, &mut provider);
        consumer.reconcile().unwrap();
        let id = consumer.bindings.keys().next().unwrap().clone();
        assert!(
            consumer
                .send("consumer", 1, &id, serde_json::json!({}), |_, _, _| Ok(()))
                .is_err()
        );
        transfer(&mut consumer, &mut provider);
        assert_eq!(provider.bindings[&id].status, "accepted");
        transfer(&mut provider, &mut consumer);
        let events = transfer(&mut consumer, &mut provider);
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Bound {
                role: Role::Consumer,
                ..
            }
        )));
        assert_eq!(provider.bindings[&id].status, "ready");
        assert!(
            consumer
                .send("consumer", 2, &id, serde_json::json!({}), |_, _, _| Ok(()))
                .is_err()
        );
        assert!(
            consumer
                .send(
                    "consumer",
                    1,
                    &id,
                    serde_json::json!({}),
                    |_, _, _| anyhow::bail!("schema rejected")
                )
                .is_err()
        );
        consumer
            .send(
                "consumer",
                1,
                &id,
                serde_json::json!({"ping": true}),
                |_, _, _| Ok(()),
            )
            .unwrap();
        transfer(&mut consumer, &mut provider);
        assert!(
            transfer(&mut provider, &mut consumer)
                .iter()
                .any(|event| matches!(event, Event::Message { .. }))
        );
        consumer.disconnect("transport");
        assert!(consumer.bindings.is_empty());
        assert!(
            consumer
                .send("consumer", 1, &id, serde_json::json!({}), |_, _, _| Ok(()))
                .is_err()
        );
    }
    #[test]
    fn expired_provider_preserves_loss_through_consumer_retirement() {
        let mut consumer = broker(false, 1);
        let mut provider = broker(true, 1);
        let mut local = record(false, 1);
        local.declaration.requires[0].disconnect = "stop".into();
        consumer.sync(&[local.clone()]);
        observe(&mut consumer, &mut provider);
        consumer.reconcile().unwrap();
        transfer(&mut consumer, &mut provider);
        transfer(&mut provider, &mut consumer);
        transfer(&mut consumer, &mut provider);
        assert_eq!(consumer.bindings.values().next().unwrap().status, "ready");
        consumer.routes.get_mut("transport").unwrap().last_seen = std::time::Instant::now()
            .checked_sub(OBSERVATION_LEASE + std::time::Duration::from_secs(1))
            .unwrap();
        consumer.reconcile().unwrap();
        assert!(consumer.bindings.is_empty());
        assert!(!consumer.routes["transport"].connected);
        let mut desired = local.declaration;
        desired.id = contract::actor_id(&desired.identity.package, &desired.identity.artifact);
        let unavailable =
            consumer.unavailable_services(Realm::PresentationClient, "1", &[desired.clone()]);
        assert_eq!(unavailable[&desired.id], vec!["echo"]);
        consumer.sync(&[]);
        assert_eq!(
            consumer.unavailable_services(Realm::PresentationClient, "1", &[desired.clone()]),
            unavailable
        );
        consumer.connect("transport", None);
        provider.publish().unwrap();
        transfer(&mut provider, &mut consumer);
        assert!(
            consumer
                .unavailable_services(Realm::PresentationClient, "1", &[desired])
                .is_empty()
        );
    }

    #[test]
    fn blocked_delivery_does_not_starve_other_lanes_or_reorder_its_messages() {
        let mut broker = Broker::default();
        for index in 0..100 {
            broker.error("blocked", 1, index.to_string());
        }
        broker.error("healthy", 2, "reply".into());
        let mut blocked = BTreeSet::new();
        let first = broker.pop_unblocked(&blocked).unwrap();
        blocked.insert(first.lane());
        broker.retry(first);
        let ready = broker.pop_unblocked(&blocked).unwrap();
        assert!(matches!(ready, Delivery::Guest { actor, .. } if actor == "healthy"));
        broker.error("later-healthy", 3, "next".into());
        broker.defer_lanes(&blocked);
        assert!(
            matches!(broker.pop().unwrap(), Delivery::Guest { actor, .. } if actor == "later-healthy")
        );
        for index in 0..100 {
            assert!(
                matches!(broker.pop().unwrap(), Delivery::Guest { event: Event::Error { message, .. }, .. } if message == index.to_string())
            );
        }
    }

    #[test]
    fn incompatible_versions_remain_pending_and_scope_is_enforced() {
        let mut consumer = broker(false, 2);
        let mut provider = broker(true, 1);
        observe(&mut consumer, &mut provider);
        consumer.reconcile().unwrap();
        assert!(consumer.bindings.is_empty());
        let mut consumer = broker(false, 1);
        provider.connect("transport", Some("2".into()));
        observe(&mut consumer, &mut provider);
        consumer.reconcile().unwrap();
        assert!(consumer.bindings.is_empty());
    }
}
