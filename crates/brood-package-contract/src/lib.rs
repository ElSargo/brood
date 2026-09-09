//! Portable package activation messages shared by realm init actors and hosts.
//!
//! This contract names immutable installed artifacts, not filesystem authority.
//! Only the host-originated package mailbox may supply desired declarations.
pub mod introspection;
pub mod services;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Content marker shared by the installer and the existing artifact exchange.
pub const BUNDLE_MAGIC: &[u8] = b"BROODPKG1\n";

/// Versioned host contracts implemented by this build. WIT container ABI is
/// checked separately when resolving each artifact.
/// Units for declared resource-use expectations. Runtime grants and observed
/// usage remain separate facts; memory sampling is not available in every host.
pub const RESOURCE_UNITS: &[(&str, &str)] = &[
    ("resources", "count"),
    ("streams", "count"),
    ("children", "count"),
    ("timers", "count"),
    ("memory-bytes", "bytes"),
];

pub const HOST_INTERFACES: &[(&str, u32)] = &[
    ("brood.package-activation", 1),
    ("brood.package-services", 1),
    ("brood.introspection", 1),
    ("brood.package-diagnostics", 1),
];

pub fn incompatible_host(requirements: &BTreeMap<String, Vec<u32>>) -> Option<String> {
    requirements.iter().find_map(|(id, versions)| {
        match HOST_INTERFACES.iter().find(|(supported, _)| supported == id) {
            Some((_, version)) if versions.contains(version) => None,
            Some((_, version)) => Some(format!("host interface {id} supports version {version}; package requires one of {versions:?}")),
            None => Some(format!("host interface {id} is unavailable")),
        }
    })
}

pub const VERSION: u32 = 1;
pub const HOST_SENDER: &str = "host:packages";
pub const MAX_ACTORS: usize = 256;
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum Realm {
    Daemon,
    SurfaceSession,
    PresentationClient,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentity {
    pub package: String,
    pub artifact: String,
    pub installation: String,
    pub artifact_digest: String,
    pub knowledge_digest: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    pub id: String,
    pub version: u32,
    pub schema_digest: String,
    pub multiplicity: String,
    pub disconnect: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DesiredActor {
    pub id: String,
    pub identity: ArtifactIdentity,
    pub capabilities: Vec<String>,
    pub provides: Vec<Interface>,
    pub requires: Vec<Interface>,
    pub restart_on_failure: bool,
    pub state_compatibility: String,
    pub migration: String,
    #[serde(default)]
    pub resource_expectations: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

impl DesiredActor {
    pub fn reference(&self) -> String {
        format!(
            "package://{}/{}",
            self.identity.installation, self.identity.artifact
        )
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RealmUpdate {
    pub version: u32,
    pub revision: String,
    /// Monotonic host observation counter. Init retry backoff uses this clock.
    pub tick: u64,
    pub actors: Vec<DesiredActor>,
    /// Host facts about previously bound required services that are now unavailable.
    #[serde(default)]
    pub unavailable_services: BTreeMap<String, Vec<String>>,
}

impl RealmUpdate {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err("package declaration exceeds mailbox limit".into());
        }
        let update: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if update.version != VERSION {
            return Err("unsupported package declaration version".into());
        }
        if update.actors.len() > MAX_ACTORS {
            return Err("too many package declarations".into());
        }
        let mut ids = BTreeSet::new();
        for actor in &update.actors {
            if actor.id != actor_id(&actor.identity.package, &actor.identity.artifact)
                || !ids.insert(&actor.id)
            {
                return Err("package actor identity is invalid or duplicated".into());
            }
        }
        Ok(update)
    }
}

pub fn actor_id(package: &str, artifact: &str) -> String {
    format!("package:{package}:{artifact}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Start(Box<DesiredActor>),
    Stop(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Child {
    Started(String),
    Stopped(String),
    Failed(String),
}

#[derive(Clone, Debug)]
enum Phase {
    Starting,
    Running,
    Stopping,
    Failed { retry_after: u64 },
}

#[derive(Clone, Debug)]
struct Instance {
    declaration: DesiredActor,
    phase: Phase,
    failures: u32,
}

/// Opt-in realm-root policy. The host provides mechanisms and observations;
/// this state machine, executing inside the root, owns package child topology.
#[derive(Default)]
pub struct InitPolicy {
    desired: BTreeMap<String, DesiredActor>,
    instances: BTreeMap<String, Instance>,
    tick: u64,
    revision: String,
}

impl InitPolicy {
    pub fn update(&mut self, mut update: RealmUpdate) -> Vec<Action> {
        for actor in &mut update.actors {
            if let Some(unavailable) = update.unavailable_services.get(&actor.id) {
                let stopped: Vec<_> = actor
                    .requires
                    .iter()
                    .filter(|interface| {
                        interface.disconnect == "stop" && unavailable.contains(&interface.id)
                    })
                    .map(|interface| interface.id.clone())
                    .collect();
                if !stopped.is_empty() && actor.blocked.is_none() {
                    actor.blocked = Some(format!(
                        "required service disconnected: {}",
                        stopped.join(", ")
                    ));
                }
            }
        }
        self.tick = self.tick.max(update.tick);
        self.revision = update.revision;
        self.desired = update
            .actors
            .into_iter()
            .map(|actor| (actor.id.clone(), actor))
            .collect();
        self.reconcile()
    }

    pub fn owns(&self, id: &str) -> bool {
        self.instances.contains_key(id) || self.desired.contains_key(id)
    }

    pub fn child(&mut self, event: Child) -> Vec<Action> {
        match event {
            Child::Started(id) => {
                if let Some(instance) = self.instances.get_mut(&id) {
                    // A changed desired generation will be stopped by reconcile.
                    if !matches!(instance.phase, Phase::Stopping) {
                        instance.phase = Phase::Running;
                    }
                }
            }
            Child::Stopped(id) => {
                if self
                    .instances
                    .get(&id)
                    .is_some_and(|instance| matches!(instance.phase, Phase::Stopping))
                {
                    self.instances.remove(&id);
                } else if let Some(instance) = self.instances.get_mut(&id) {
                    instance.phase = Phase::Failed {
                        retry_after: self.tick.saturating_add(2),
                    };
                }
            }
            Child::Failed(id) => {
                if let Some(instance) = self.instances.get_mut(&id) {
                    instance.failures = instance.failures.saturating_add(1);
                    instance.phase = Phase::Failed {
                        retry_after: self.tick.saturating_add(1u64 << instance.failures.min(6)),
                    };
                }
            }
        }
        self.reconcile()
    }

    pub fn report(&self) -> PolicyReport {
        PolicyReport {
            version: VERSION,
            revision: self.revision.clone(),
            actors: self
                .desired
                .values()
                .map(|desired| PolicyActor {
                    id: desired.id.clone(),
                    installation: desired.identity.installation.clone(),
                    status: if desired.blocked.is_some() {
                        "pending"
                    } else if let Some(instance) = self.instances.get(&desired.id) {
                        match instance.phase {
                            Phase::Starting => "starting",
                            Phase::Running => "running",
                            Phase::Stopping => "stopping",
                            Phase::Failed { .. } => "failed",
                        }
                    } else {
                        "pending"
                    }
                    .into(),
                    reason: desired.blocked.clone(),
                })
                .collect(),
        }
    }

    fn reconcile(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        for (id, instance) in &mut self.instances {
            let replacement = self
                .desired
                .get(id)
                .is_none_or(|desired| desired != &instance.declaration);
            let retry = matches!(instance.phase, Phase::Failed { retry_after } if self.tick >= retry_after)
                && instance.declaration.restart_on_failure;
            // Starting is also retired through the root's stop API; the host
            // already owns cancellation of a child that is still loading.
            if (replacement || retry) && !matches!(instance.phase, Phase::Stopping) {
                instance.phase = Phase::Stopping;
                actions.push(Action::Stop(id.clone()));
            }
        }
        for (id, desired) in &self.desired {
            if desired.blocked.is_none() && !self.instances.contains_key(id) {
                self.instances.insert(
                    id.clone(),
                    Instance {
                        declaration: desired.clone(),
                        phase: Phase::Starting,
                        failures: 0,
                    },
                );
                actions.push(Action::Start(Box::new(desired.clone())));
            }
        }
        actions
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyReport {
    #[serde(rename = "brood-package-policy")]
    pub version: u32,
    pub revision: String,
    pub actors: Vec<PolicyActor>,
}
impl PolicyReport {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err("root policy report exceeds mailbox limit".into());
        }
        Ok(bytes)
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyActor {
    pub id: String,
    pub installation: String,
    pub status: String,
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn actor(digest: &str) -> DesiredActor {
        DesiredActor {
            id: actor_id("org.example.test", "actor"),
            identity: ArtifactIdentity {
                package: "org.example.test".into(),
                artifact: "actor".into(),
                installation: digest.into(),
                artifact_digest: digest.into(),
                knowledge_digest: digest.into(),
            },
            capabilities: vec![],
            provides: vec![],
            requires: vec![],
            restart_on_failure: false,
            state_compatibility: "stateless".into(),
            migration: "none".into(),
            resource_expectations: BTreeMap::new(),
            blocked: None,
        }
    }
    fn update(actor: DesiredActor) -> RealmUpdate {
        RealmUpdate {
            version: VERSION,
            revision: actor.identity.installation.clone(),
            tick: 1,
            actors: vec![actor],
            unavailable_services: BTreeMap::new(),
        }
    }
    #[test]
    fn replacement_waits_for_teardown_and_repeat_never_duplicates() {
        let mut policy = InitPolicy::default();
        let first = actor("old");
        assert_eq!(
            policy.update(update(first.clone())),
            vec![Action::Start(Box::new(first.clone()))]
        );
        assert!(policy.update(update(first.clone())).is_empty());
        assert!(policy.child(Child::Started(first.id.clone())).is_empty());
        let second = actor("new");
        assert_eq!(
            policy.update(update(second.clone())),
            vec![Action::Stop(first.id.clone())]
        );
        assert!(policy.update(update(second.clone())).is_empty());
        assert_eq!(
            policy.child(Child::Stopped(first.id)),
            vec![Action::Start(Box::new(second))]
        );
    }
    #[test]
    fn failed_actor_stays_failed_until_retry_policy_or_replacement() {
        let mut policy = InitPolicy::default();
        let first = actor("old");
        policy.update(update(first.clone()));
        assert!(policy.child(Child::Failed(first.id.clone())).is_empty());
        assert!(policy.update(update(first.clone())).is_empty());
        assert_eq!(
            policy.update(RealmUpdate {
                actors: vec![],
                unavailable_services: BTreeMap::new(),
                ..update(first.clone())
            }),
            vec![Action::Stop(first.id)]
        );
    }
    #[test]
    fn root_stops_lost_service_consumer_until_provider_returns() {
        let mut policy = InitPolicy::default();
        let mut consumer = actor("same");
        consumer.requires.push(Interface {
            id: "echo".into(),
            version: 1,
            schema_digest: "schema".into(),
            multiplicity: "one".into(),
            disconnect: "stop".into(),
        });
        policy.update(update(consumer.clone()));
        policy.child(Child::Started(consumer.id.clone()));
        let mut unavailable = update(consumer.clone());
        unavailable
            .unavailable_services
            .insert(consumer.id.clone(), vec!["echo".into()]);
        assert_eq!(
            policy.update(unavailable.clone()),
            vec![Action::Stop(consumer.id.clone())]
        );
        assert!(policy.child(Child::Stopped(consumer.id.clone())).is_empty());
        assert!(policy.update(unavailable).is_empty());
        assert_eq!(
            policy.report().actors[0].reason.as_deref(),
            Some("required service disconnected: echo")
        );
        assert_eq!(
            policy.update(update(consumer.clone())),
            vec![Action::Start(Box::new(consumer))]
        );
    }

    #[test]
    fn incompatible_state_retires_old_code_and_reports_why_start_is_pending() {
        let mut policy = InitPolicy::default();
        let old = actor("old");
        policy.update(update(old.clone()));
        policy.child(Child::Started(old.id.clone()));
        let mut next = actor("new");
        next.blocked = Some("manual migration required".into());
        assert_eq!(
            policy.update(update(next.clone())),
            vec![Action::Stop(old.id.clone())]
        );
        assert!(policy.child(Child::Stopped(old.id)).is_empty());
        assert_eq!(
            policy.report().actors[0].reason.as_deref(),
            Some("manual migration required")
        );
        assert!(policy.update(update(next.clone())).is_empty());
        next.blocked = None;
        assert_eq!(
            policy.update(update(next.clone())),
            vec![Action::Start(Box::new(next))]
        );
    }
}
