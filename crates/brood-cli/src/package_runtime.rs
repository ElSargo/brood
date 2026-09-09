//! Host mechanisms for observing selected packages without doing filesystem work
//! on compositor or presentation turns. Realm roots consume the declarations.
use std::{
    env,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use brood_package_manifest::catalogue;
pub use brood_package_manifest::{catalogue::Catalogue, contract};

#[must_use]
pub fn default_store() -> PathBuf {
    env::var_os("BROOD_PACKAGE_STORE").map_or_else(
        || {
            env::var_os("XDG_DATA_HOME")
                .map_or_else(
                    || {
                        env::var_os("HOME").map_or_else(
                            || PathBuf::from(".local/share"),
                            |home| PathBuf::from(home).join(".local/share"),
                        )
                    },
                    PathBuf::from,
                )
                .join("brood/packages")
        },
        PathBuf::from,
    )
}

pub struct CatalogueMonitor {
    receiver: mpsc::Receiver<Catalogue>,
    stop: Arc<AtomicBool>,
    worker: thread::Thread,
    pub latest: Catalogue,
}

impl CatalogueMonitor {
    pub fn start(store: PathBuf, wake: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("brood-package-catalogue".into())
            .spawn(move || {
                let mut delivered = String::new();
                let mut has_packages = false;
                let mut last_valid = Catalogue::default();
                while !worker_stop.load(Ordering::Acquire) {
                    let revision = catalogue::selection_revision(&store);
                    let changed = revision
                        .as_ref()
                        .map_or(true, |revision| revision != &delivered);
                    if changed {
                        let next = match revision.and_then(|_| catalogue::load(&store)) {
                            Ok(catalogue) => { last_valid = catalogue.clone(); catalogue },
                            Err(error) => Catalogue {
                                failures: vec![format!("package catalogue temporarily unavailable; retaining last verified declarations: {error:#}")],
                                ..last_valid.clone()
                            },
                        };
                        let revision = next.revision.clone();
                        let active = !next.packages.is_empty();
                        match sender.try_send(next) {
                            Ok(()) => {
                                delivered = revision;
                                // Keep endpoint observation and teardown acknowledgements alive
                                // after the last selection is removed.
                                has_packages |= active;
                                wake();
                            }
                            Err(mpsc::TrySendError::Full(_)) => {}
                            Err(mpsc::TrySendError::Disconnected(_)) => break,
                        }
                    }
                    if has_packages {
                        wake();
                    }
                    thread::park_timeout(Duration::from_millis(250));
                }
            })?;
        let worker = worker.thread().clone();
        Ok(Self {
            receiver,
            stop,
            worker,
            latest: Catalogue::default(),
        })
    }

    pub fn poll(&mut self) {
        while let Ok(catalogue) = self.receiver.try_recv() {
            self.latest = catalogue;
        }
    }
}

impl Drop for CatalogueMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.unpark();
    }
}

#[derive(Clone, Debug, serde::Serialize, schemars::JsonSchema)]
pub struct InstanceRecord {
    pub realm: contract::Realm,
    pub scope: String,
    pub actor: String,
    pub owner: String,
    pub declaration: contract::DesiredActor,
    pub generation: Option<u64>,
    pub lifecycle: String,
    pub effective_grants: Vec<String>,
    pub resources: std::collections::BTreeMap<String, u64>,
    pub resources_observed_at_unix_ms: Option<u64>,
    pub recent_failures: Vec<String>,
    pub diagnostics: std::collections::BTreeMap<String, serde_json::Value>,
    pub resource_assessment: std::collections::BTreeMap<String, serde_json::Value>,
}

impl InstanceRecord {
    pub fn assess_resources(&mut self) {
        self.resource_assessment = self.declaration.resource_expectations.iter().map(|(resource, expected)| {
            let observed = self.resources.get(resource);
            let status = match observed {
                Some(observed) if observed > expected => "above-expectation",
                Some(_) => "within-expectation",
                None if self.resources_observed_at_unix_ms.is_some() => "unsupported-observation",
                None => "not-observed",
            };
            (resource.clone(), serde_json::json!({"expected": expected, "observed": observed, "status": status, "authority": "declared-expectation-and-host-observation"}))
        }).collect();
    }
}

/// Local observable facts and validated artifact lookup. This object never
/// issues actor start/stop commands; the realm init policy owns those decisions.
pub struct PackageHost {
    monitor: Option<CatalogueMonitor>,
    startup_failure: Option<String>,
    policy_reports: std::cell::RefCell<std::collections::BTreeMap<String, serde_json::Value>>,
    records: std::cell::RefCell<std::collections::BTreeMap<String, InstanceRecord>>,
    pub services: std::cell::RefCell<crate::package_services::Broker>,
    tick: u64,
    last_tick: std::time::Instant,
}

impl PackageHost {
    pub fn new(wake: impl Fn() + Send + 'static) -> Self {
        let (monitor, startup_failure) = match CatalogueMonitor::start(default_store(), wake) {
            Ok(monitor) => (Some(monitor), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            monitor,
            startup_failure,
            policy_reports: std::cell::RefCell::new(std::collections::BTreeMap::new()),
            records: std::cell::RefCell::new(std::collections::BTreeMap::new()),
            services: std::cell::RefCell::new(crate::package_services::Broker::default()),
            tick: 0,
            last_tick: std::time::Instant::now(),
        }
    }

    /// Bounded tick: only receive an already verified background catalogue.
    pub fn poll(&mut self) -> bool {
        let changed = if let Some(monitor) = &mut self.monitor {
            let before = monitor.latest.revision.clone();
            monitor.poll();
            before != monitor.latest.revision
        } else {
            false
        };
        if changed || self.last_tick.elapsed() >= Duration::from_millis(500) {
            self.tick = self.tick.saturating_add(1);
            self.last_tick = std::time::Instant::now();
            true
        } else {
            false
        }
    }

    pub fn update(&self, realm: contract::Realm, scope: &str) -> anyhow::Result<Vec<u8>> {
        use anyhow::Context;
        let catalogue = &self
            .monitor
            .as_ref()
            .context("package catalogue unavailable")?
            .latest;
        let key = record_key(realm, scope, "policy");
        let mut reports = self.policy_reports.borrow_mut();
        let entry = reports.entry(key).or_insert_with(|| serde_json::json!({"realm": realm, "scope": scope, "status": "awaiting-root-acknowledgement", "reason": "root may be unavailable or may not support package policy", "authority": "host-observed"}));
        if let Some(entry) = entry.as_object_mut() {
            if entry
                .get("report")
                .and_then(|report| report.get("revision"))
                .and_then(serde_json::Value::as_str)
                != Some(catalogue.revision.as_str())
            {
                entry.insert(
                    "status".into(),
                    serde_json::json!("awaiting-root-acknowledgement"),
                );
            }
            entry.insert(
                "desired_revision".into(),
                serde_json::json!(catalogue.revision),
            );
        }
        drop(reports);
        let actors = catalogue.desired(realm, scope)?;
        let unavailable_services = self
            .services
            .borrow_mut()
            .unavailable_services(realm, scope, &actors);
        let update = contract::RealmUpdate {
            version: contract::VERSION,
            revision: catalogue.revision.clone(),
            tick: self.tick,
            actors,
            unavailable_services,
        };
        let bytes = serde_json::to_vec(&update)?;
        anyhow::ensure!(
            bytes.len() <= contract::MAX_MESSAGE_BYTES,
            "realm package declaration exceeds mailbox limit"
        );
        Ok(bytes)
    }

    pub fn resolve(
        &self,
        realm: contract::Realm,
        scope: &str,
        id: &str,
        reference: &str,
        capabilities: &[String],
    ) -> anyhow::Result<(PathBuf, contract::DesiredActor)> {
        use anyhow::Context;
        self.monitor
            .as_ref()
            .context("package catalogue unavailable")?
            .latest
            .resolve(realm, scope, id, reference, capabilities)
    }

    pub fn starting(
        &self,
        realm: contract::Realm,
        scope: &str,
        actor: &str,
        owner: &str,
        declaration: contract::DesiredActor,
    ) {
        let mut record = InstanceRecord {
            realm,
            scope: scope.into(),
            actor: actor.into(),
            owner: owner.into(),
            declaration,
            generation: None,
            lifecycle: "starting".into(),
            effective_grants: vec![],
            resources: std::collections::BTreeMap::new(),
            resources_observed_at_unix_ms: None,
            recent_failures: vec![],
            diagnostics: std::collections::BTreeMap::new(),
            resource_assessment: std::collections::BTreeMap::new(),
        };
        record.assess_resources();
        if let Some(package) = self.monitor.as_ref().and_then(|monitor| {
            monitor.latest.packages.iter().find(|package| {
                package.installation.plan_digest == record.declaration.identity.installation
            })
        }) {
            record.diagnostics = package.diagnostic_validators.iter().map(|(id, (version, digest, _))| (id.clone(), serde_json::json!({"id": id, "schema_version": version, "schema_digest": digest, "authority": "plugin-reported", "status": "not-reported", "validation": "not-observed", "observed_at_unix_ms": null, "value": null, "error": null}))).collect();
        }
        self.records
            .borrow_mut()
            .insert(record_key(realm, scope, &record.declaration.id), record);
    }

    pub fn started(&self, realm: contract::Realm, scope: &str, id: &str, generation: u64) {
        if let Some(record) = self
            .records
            .borrow_mut()
            .get_mut(&record_key(realm, scope, id))
        {
            record.generation = Some(generation);
            record.lifecycle = "running".into();
            record
                .effective_grants
                .clone_from(&record.declaration.capabilities);
        }
    }

    pub fn outcome(
        &self,
        realm: contract::Realm,
        scope: &str,
        id: &str,
        lifecycle: &str,
        failure: Option<&str>,
    ) {
        if let Some(record) = self
            .records
            .borrow_mut()
            .get_mut(&record_key(realm, scope, id))
        {
            record.lifecycle = lifecycle.into();
            if let Some(failure) = failure {
                if record.recent_failures.len() == 8 {
                    record.recent_failures.remove(0);
                }
                record.recent_failures.push(failure.into());
            }
            if lifecycle == "stopped" {
                record.resources.clear();
                record.resources_observed_at_unix_ms = None;
                record.assess_resources();
                record.effective_grants.clear();
            }
        }
    }

    #[must_use]
    pub fn owns(&self, realm: contract::Realm, scope: &str, id: &str) -> bool {
        self.records
            .borrow()
            .contains_key(&record_key(realm, scope, id))
    }

    #[must_use]
    pub fn records(&self) -> Vec<InstanceRecord> {
        self.records.borrow().values().cloned().collect()
    }

    pub fn policy_report(
        &self,
        realm: contract::Realm,
        scope: &str,
        root: &str,
        generation: u64,
        payload: &[u8],
    ) -> bool {
        if payload.len() > contract::MAX_MESSAGE_BYTES {
            return false;
        }
        let Ok(report) = serde_json::from_slice::<contract::PolicyReport>(payload) else {
            return false;
        };
        if report.version != contract::VERSION || report.actors.len() > contract::MAX_ACTORS {
            return false;
        }
        self.policy_reports.borrow_mut().insert(record_key(realm, scope, "policy"), serde_json::json!({"realm": realm, "scope": scope, "root": root, "generation": generation, "authority": "root-reported", "status": if self.monitor.as_ref().is_some_and(|monitor| monitor.latest.revision == report.revision) {"acknowledged"} else {"stale-acknowledgement"}, "observed_at_unix_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_millis()), "report": report}));
        true
    }

    pub fn sync_services(&self) {
        self.services.borrow_mut().sync(&self.records());
    }

    fn report_diagnostic(
        &self,
        actor: &str,
        generation: u64,
        id: &str,
        version: u32,
        value: Option<serde_json::Value>,
        failure: Option<String>,
    ) -> anyhow::Result<()> {
        use anyhow::{Context, ensure};
        let mut records = self.records.borrow_mut();
        let record = records
            .values_mut()
            .find(|record| record.actor == actor && record.generation == Some(generation))
            .context("diagnostic actor generation unavailable")?;
        let catalogue = &self
            .monitor
            .as_ref()
            .context("diagnostic catalogue unavailable")?
            .latest;
        let package = catalogue
            .packages
            .iter()
            .find(|package| {
                package.installation.plan_digest == record.declaration.identity.installation
            })
            .context("diagnostic installation no longer selected")?;
        let (expected, digest, validator) = package
            .diagnostic_validators
            .get(id)
            .context("diagnostic ID is not declared by this package")?;
        let validation = (|| -> anyhow::Result<()> {
            ensure!(version == *expected, "diagnostic schema version mismatch");
            ensure!(
                value.is_some() != failure.is_some(),
                "report requires exactly one value or failure"
            );
            if let Some(failure) = &failure {
                ensure!(failure.len() <= 2048, "diagnostic failure exceeds limit");
            }
            if let Some(value) = &value {
                ensure!(
                    serde_json::to_vec(value)?.len() <= 4096,
                    "diagnostic value exceeds 4096 bytes"
                );
                validator.validate(value).map_err(|error| {
                    anyhow::anyhow!("diagnostic schema validation failed: {error}")
                })?;
            }
            Ok(())
        })();
        let validation_error = validation.err().map(|error| error.to_string());
        let rejected = validation_error.is_some();
        let error = validation_error.or(failure);
        let entry = serde_json::json!({"id": id, "schema_version": expected, "schema_digest": digest, "authority": "plugin-reported", "validation": if rejected {"rejected"} else {"accepted"}, "observed_at_unix_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_millis()), "status": if error.is_some() {"failed"} else {"reported"}, "value": if error.is_none() {value} else {None}, "error": error});
        let mut updated = record.diagnostics.clone();
        updated.insert(id.into(), entry);
        ensure!(
            serde_json::to_vec(&updated)?.len() <= 12 * 1024,
            "aggregate diagnostic budget exceeded"
        );
        record.diagnostics = updated;
        Ok(())
    }

    pub fn observe_resources(
        &self,
        samples: Vec<(String, u64, std::collections::BTreeMap<String, u64>)>,
    ) {
        let observed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| {
                u64::try_from(time.as_millis()).unwrap_or(u64::MAX)
            });
        for (actor, generation, resources) in samples {
            if let Some(record) = self.records.borrow_mut().values_mut().find(|record| {
                record.actor == actor
                    && record.generation == Some(generation)
                    && record.lifecycle == "running"
            }) {
                record.resources = resources;
                record.resources_observed_at_unix_ms = Some(observed);
                record.assess_resources();
            }
        }
    }

    pub fn service_request(
        &self,
        actor: &str,
        generation: u64,
        payload: &[u8],
        sample_resources: impl FnOnce() -> Vec<(String, u64, std::collections::BTreeMap<String, u64>)>,
    ) -> bool {
        if payload.len() > contract::services::MAX_WIRE_BYTES
            || serde_json::from_slice::<serde_json::Value>(payload)
                .ok()
                .is_none_or(|value| value.get("brood-service").is_none())
        {
            return false;
        }
        if !self
            .records
            .borrow()
            .values()
            .any(|record| record.actor == actor && record.generation == Some(generation))
        {
            return false;
        }
        self.sync_services();
        let mut broker = self.services.borrow_mut();
        let result = contract::services::Request::decode(payload)
            .map_err(anyhow::Error::msg)
            .and_then(|request| match request {
                contract::services::Request::ReportDiagnostic {
                    id,
                    schema_version,
                    value,
                    failure,
                    ..
                } => self.report_diagnostic(actor, generation, &id, schema_version, value, failure),
                contract::services::Request::Inspect {
                    request_id, query, ..
                } => {
                    self.observe_resources(sample_resources());
                    let records = self.records();
                    let caller = records
                        .iter()
                        .find(|record| {
                            record.actor == actor && record.generation == Some(generation)
                        })
                        .ok_or_else(|| anyhow::anyhow!("caller generation is unavailable"))?;
                    let services = broker.describe();
                    let response = crate::introspection::guest_query(
                        &query,
                        caller.realm,
                        &caller.scope,
                        &serde_json::to_value(&records)?,
                        &self.query_catalogue(&query),
                        &services,
                    );
                    broker.inspection(actor, generation, request_id, response)
                }

                contract::services::Request::Register { interfaces, .. } => {
                    broker.register(actor, generation, interfaces)
                }
                contract::services::Request::Send {
                    binding, payload, ..
                } => broker.send(
                    actor,
                    generation,
                    &binding,
                    payload,
                    |peer, interface, payload| self.validate_payload(peer, interface, payload),
                ),
            });
        if let Err(error) = result {
            broker.error(actor, generation, format!("{error:#}"));
        }
        true
    }

    fn validate_payload(
        &self,
        peer: &contract::services::Peer,
        interface: &contract::Interface,
        payload: &serde_json::Value,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let package = self
            .monitor
            .as_ref()
            .context("catalogue unavailable")?
            .latest
            .packages
            .iter()
            .find(|package| package.installation.plan_digest == peer.identity.installation)
            .context("bound installation no longer selected")?;
        let validator = package
            .validators
            .get(&interface.id)
            .context("installed interface schema unavailable")?;
        validator
            .validate(payload)
            .map_err(|error| anyhow::anyhow!("service payload violates installed schema: {error}"))
    }

    pub fn service_packet(&self, route: &str, payload: &[u8]) -> anyhow::Result<()> {
        let packet = contract::services::Packet::decode(payload).map_err(anyhow::Error::msg)?;
        self.sync_services();
        self.services
            .borrow_mut()
            .receive(route, packet, |peer, interface, payload| {
                self.validate_payload(peer, interface, payload)
            })
    }

    /// Only already-verified selected knowledge is served on runtime turns.
    pub fn query_catalogue(&self, query: &serde_json::Value) -> serde_json::Value {
        let mut description = self.describe();
        let package = query.get("package").and_then(serde_json::Value::as_str);
        let revision = query
            .get("installation")
            .and_then(serde_json::Value::as_str);
        if let Some(package) = self.monitor.as_ref().and_then(|monitor| {
            monitor.latest.packages.iter().find(|entry| {
                Some(entry.installation.package_id.as_str()) == package
                    && Some(entry.installation.plan_digest.as_str()) == revision
            })
        }) && let Some(description) = description.as_object_mut()
        {
            description.insert("knowledge".into(), (*package.knowledge).clone());
            description.insert(
                "knowledge_digest".into(),
                serde_json::json!(package.installation.knowledge_digest),
            );
        }
        description
    }

    #[must_use]
    pub fn describe(&self) -> serde_json::Value {
        let catalogue = self.monitor.as_ref().map(|monitor| &monitor.latest);
        serde_json::json!({"revision": catalogue.map(|catalogue| &catalogue.revision),
            "installations": catalogue.map(|catalogue| catalogue.packages.iter().map(|package| serde_json::json!({"installation": package.installation, "enabled": package.enabled, "destination_authorization": package.authorization, "state": package.state})).collect::<Vec<_>>()),
            "failures": catalogue.map(|catalogue| &catalogue.failures), "startup_failure": self.startup_failure, "root_policy": self.policy_reports.borrow().values().collect::<Vec<_>>()})
    }
}

fn record_key(realm: contract::Realm, scope: &str, id: &str) -> String {
    format!("{realm:?}/{scope}/{id}")
}

/// Import runs on transport workers, never on a compositor/presentation turn.
pub fn import_remote_bundle(
    package: &str,
    input: &std::path::Path,
    realm: contract::Realm,
    store: &std::path::Path,
) -> anyhow::Result<serde_json::Value> {
    brood_package_manifest::installation::import_bundle(
        store,
        package,
        input,
        brood_package_manifest::installation::DestinationAuthorization {
            realms: vec![realm],
            capabilities: vec![],
            source: "destination artifact allowlist on authenticated connection".into(),
        },
    )
}
