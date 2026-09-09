//! Verified host-side package catalogue, independent of realm loading policy.
use crate::{
    Manifest,
    extension::Realm,
    installation::{self, Installation},
};
use anyhow::{Context, Result, ensure};
use brood_package_contract::{ArtifactIdentity, DesiredActor, Interface, MAX_ACTORS, actor_id};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Package {
    pub enabled: bool,
    pub authorization: Option<installation::DestinationAuthorization>,
    pub state: Option<installation::StateRecord>,
    pub manifest: Manifest,
    pub installation: Installation,
    pub root: PathBuf,
    pub knowledge: std::sync::Arc<serde_json::Value>,
    pub interfaces: BTreeMap<String, Interface>,
    pub validators: BTreeMap<String, std::sync::Arc<jsonschema::Validator>>,
    pub diagnostic_validators:
        BTreeMap<String, (u32, String, std::sync::Arc<jsonschema::Validator>)>,
}

#[derive(Clone, Debug, Default)]
pub struct Catalogue {
    pub revision: String,
    pub packages: Vec<Package>,
    pub failures: Vec<String>,
}

/// Cheap selection observation. Artifact hashing happens only after a change,
/// on a host background worker; it never runs on a compositor turn.
pub fn selection_revision(store: &Path) -> Result<String> {
    let selections = store.join("selected");
    if !selections.exists() {
        return Ok(installation::digest(b"empty catalogue"));
    }
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(selections)? {
        ensure!(
            entries.len() < MAX_ACTORS,
            "selected package count exceeds endpoint limit"
        );
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_file(),
            "selection must be a regular file"
        );
        let value = fs::read_to_string(entry.path())?;
        ensure!(value.len() <= 65, "selection record exceeds limit");
        entries.insert(
            entry.file_name().to_string_lossy().into_owned(),
            (
                value.clone(),
                store.join("disabled").join(entry.file_name()).exists(),
                installation::destination_authorization(store, value.trim())?,
                installation::state_record(store, &entry.file_name().to_string_lossy())?,
            ),
        );
    }
    Ok(installation::digest(&serde_json::to_vec(&entries)?))
}

pub fn load(store: &Path) -> Result<Catalogue> {
    let _lock = installation::store_lock(store, true)?;
    ensure!(
        !store.join("pending-publication.json").exists(),
        "package publication was interrupted; recover the store before observing a new catalogue"
    );
    load_while_locked(store)
}

pub(crate) fn load_while_locked(store: &Path) -> Result<Catalogue> {
    let revision = selection_revision(store)?;
    let mut catalogue = Catalogue {
        revision,
        ..Catalogue::default()
    };
    let selections = store.join("selected");
    if !selections.exists() {
        return Ok(catalogue);
    }
    let mut names: Vec<_> = fs::read_dir(selections)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<_>>()?;
    names.sort();
    for name in names {
        let name = name.to_str().context("non-UTF8 package ID")?;
        match load_package(store, name) {
            Ok(package) => catalogue.packages.push(package),
            Err(error) => catalogue.failures.push(format!("{name}: {error:#}")),
        }
    }
    ensure!(
        catalogue.revision == selection_revision(store)?,
        "selection changed while loading catalogue"
    );
    Ok(catalogue)
}

fn load_package(store: &Path, name: &str) -> Result<Package> {
    let installation = installation::inspect(store, name)?;
    let root = store
        .join("objects")
        .join(&installation.plan_digest)
        .canonicalize()?;
    let manifest = Manifest::from_path(root.join(crate::INSTALLED_MANIFEST_FILE))?;
    let knowledge: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("brood-knowledge.json"))?)?;
    let extension = manifest
        .extension
        .as_ref()
        .context("installation has no extension contract")?;
    let interfaces = extension
        .interfaces
        .iter()
        .map(|interface| {
            let schema = knowledge
                .get("schemas")
                .and_then(|schemas| schemas.get(&interface.id))
                .context("installed interface schema missing")?;
            Ok((
                interface.id.clone(),
                Interface {
                    id: interface.id.clone(),
                    version: interface.version,
                    schema_digest: installation::digest(&serde_json::to_vec(schema)?),
                    multiplicity: interface.multiplicity.clone(),
                    disconnect: interface.disconnect.clone(),
                },
            ))
        })
        .collect::<Result<_>>()?;
    let validators = extension
        .interfaces
        .iter()
        .map(|interface| {
            let schema = knowledge
                .get("schemas")
                .and_then(|schemas| schemas.get(&interface.id))
                .context("installed interface schema missing")?;
            Ok((
                interface.id.clone(),
                std::sync::Arc::new(jsonschema::validator_for(schema)?),
            ))
        })
        .collect::<Result<_>>()?;
    let diagnostic_validators = extension
        .diagnostics
        .iter()
        .map(|diagnostic| -> Result<_> {
            let schema = knowledge
                .get("diagnostic_schemas")
                .and_then(|schemas| schemas.get(&diagnostic.id))
                .context("diagnostic schema missing")?;
            Ok((
                diagnostic.id.clone(),
                (
                    diagnostic.version,
                    installation::digest(&serde_json::to_vec(schema)?),
                    std::sync::Arc::new(jsonschema::validator_for(schema)?),
                ),
            ))
        })
        .collect::<Result<_>>()?;
    Ok(Package {
        enabled: !store.join("disabled").join(name).exists(),
        authorization: installation::destination_authorization(store, &installation.plan_digest)?,
        state: installation::state_record(store, name)?,
        manifest,
        installation,
        root,
        interfaces,
        validators,
        diagnostic_validators,
        knowledge: std::sync::Arc::new(knowledge),
    })
}

impl Catalogue {
    fn blocked(&self, package: &Package, visiting: &mut Vec<String>) -> Option<String> {
        let id = package.manifest.package.id.to_string();
        if visiting.contains(&id) {
            return Some(format!("dependency cycle at {id}"));
        }
        if !package.enabled {
            return Some(format!("package {id} is disabled"));
        }
        let extension = package.manifest.extension.as_ref()?;
        if let Some(reason) = brood_package_contract::incompatible_host(&extension.host_interfaces)
        {
            return Some(reason);
        }
        if let Some(state) = &package.state
            && state.compatibility != "stateless"
            && extension.state_compatibility != "stateless"
            && state.compatibility != extension.state_compatibility
        {
            return Some(format!(
                "state format {} is incompatible with {}; migration requirement: {}",
                state.compatibility, extension.state_compatibility, extension.migration
            ));
        }
        visiting.push(id);
        for (dependency, expected) in &package.installation.dependencies {
            let Some(selected) = self
                .packages
                .iter()
                .find(|candidate| candidate.manifest.package.id.as_str() == dependency)
            else {
                return Some(format!("dependency {dependency} is unavailable"));
            };
            if selected.installation.plan_digest != *expected {
                return Some(format!(
                    "dependency {dependency} requires installation {expected}; selected {}",
                    selected.installation.plan_digest
                ));
            }
            if let Some(reason) = self.blocked(selected, visiting) {
                return Some(format!("dependency {dependency}: {reason}"));
            }
        }
        visiting.pop();
        None
    }

    /// Instance scope is the realm. Selection may additionally restrict the
    /// declaration to one caller-supplied stable realm identifier.
    pub fn desired(&self, realm: Realm, selector: &str) -> Result<Vec<DesiredActor>> {
        let mut desired = Vec::new();
        for package in self.packages.iter().filter(|package| package.enabled) {
            let extension = package
                .manifest
                .extension
                .as_ref()
                .context("missing extension")?;
            for activation in &extension.activation {
                if package
                    .authorization
                    .as_ref()
                    .is_some_and(|authorization| !authorization.realms.contains(&realm))
                    || activation.realm != realm
                    || activation
                        .selector
                        .as_deref()
                        .is_some_and(|wanted| wanted != "*" && wanted != selector)
                {
                    continue;
                }
                ensure!(
                    desired.len() < MAX_ACTORS,
                    "package actor count exceeds realm limit"
                );
                let artifact = package
                    .manifest
                    .artifact(&activation.artifact)
                    .context("activation artifact missing")?;
                let interfaces = |ids: &[String]| -> Result<Vec<Interface>> {
                    ids.iter()
                        .map(|id| {
                            package
                                .interfaces
                                .get(id)
                                .cloned()
                                .context("interface missing")
                        })
                        .collect()
                };
                desired.push(DesiredActor {
                    id: actor_id(package.manifest.package.id.as_str(), artifact.id.as_str()),
                    identity: ArtifactIdentity {
                        package: package.manifest.package.id.to_string(),
                        artifact: artifact.id.to_string(),
                        installation: package.installation.plan_digest.clone(),
                        artifact_digest: package
                            .installation
                            .artifacts
                            .get(artifact.id.as_str())
                            .context("artifact digest missing")?
                            .clone(),
                        knowledge_digest: package.installation.knowledge_digest.clone(),
                    },
                    capabilities: artifact
                        .capabilities
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    provides: interfaces(&activation.provides)?,
                    requires: interfaces(&activation.requires)?,
                    restart_on_failure: activation.restart == "on-failure",
                    state_compatibility: extension.state_compatibility.clone(),
                    migration: extension.migration.clone(),
                    resource_expectations: activation.resource_expectations.clone(),
                    blocked: self.blocked(package, &mut Vec::new()),
                });
            }
        }
        Ok(desired)
    }

    /// Resolve only an exact selected declaration in this destination realm.
    /// A guest reference cannot authorize an unrelated object or a new init root.
    pub fn resolve(
        &self,
        realm: Realm,
        selector: &str,
        id: &str,
        reference: &str,
        capabilities: &[String],
    ) -> Result<(PathBuf, DesiredActor)> {
        let declaration = self
            .desired(realm, selector)?
            .into_iter()
            .find(|actor| actor.id == id && actor.reference() == reference)
            .context("package reference is not selected for this realm")?;
        ensure!(
            declaration.blocked.is_none(),
            "activation is blocked: {}",
            declaration.blocked.as_deref().unwrap_or_default()
        );
        let mut requested = capabilities.to_vec();
        requested.sort();
        let mut declared = declaration.capabilities.clone();
        declared.sort();
        ensure!(
            requested == declared,
            "init request differs from declared package capabilities"
        );
        let package = self
            .packages
            .iter()
            .find(|package| package.installation.plan_digest == declaration.identity.installation)
            .context("installation disappeared")?;
        let artifact = package
            .manifest
            .artifact(&declaration.identity.artifact)
            .context("artifact disappeared")?;
        Ok((package.root.join(&artifact.placement), declaration))
    }
}
