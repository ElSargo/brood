//! Optional self-description on the existing package manifest.
use crate::{Manifest, ValidationIssue, issue, validate_relative_path};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Extension {
    pub schema_version: u32,
    pub purpose: String,
    pub source_revision: String,
    pub build: Build,
    #[serde(default)]
    pub prebuilt: Vec<Prebuilt>,
    pub documents: Vec<Document>,
    pub activation: Vec<Activation>,
    #[serde(default)]
    pub interfaces: Vec<Interface>,
    #[serde(default)]
    pub configuration_schema: Option<PathBuf>,
    pub state_compatibility: String,
    pub migration: String,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default)]
    pub host_interfaces: BTreeMap<String, Vec<u32>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Diagnostic {
    pub id: String,
    pub version: u32,
    pub schema: PathBuf,
}

/// A dependency manifest is resolved beneath this source root and captured in
/// the installation plan; applying never follows the source path again.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Dependency {
    pub package: String,
    pub manifest: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Build {
    #[serde(default = "default_build_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_log_limit")]
    pub log_limit_bytes: u64,

    /// Individual source inputs, distinct from the artifact output paths.
    pub inputs: Vec<PathBuf>,
    /// Absolute executable in an immutable Nix store environment.
    pub executable: PathBuf,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

/// Ordered local alternatives, each containing the complete artifact set.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Prebuilt {
    pub id: String,
    #[serde(default)]
    pub host_interfaces: BTreeMap<String, Vec<u32>>,
    pub artifacts: BTreeMap<String, PrebuiltArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PrebuiltArtifact {
    pub path: PathBuf,
    pub sha256: String,
}

const fn default_log_limit() -> u64 {
    8 * 1024 * 1024
}

const fn default_build_timeout() -> u64 {
    900
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Document {
    pub id: String,
    pub path: PathBuf,
    pub category: String,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub schemas: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Activation {
    pub artifact: String,
    pub realm: Realm,
    pub scope: String,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub provides: Vec<String>,
    pub restart: String,
    pub deactivation: String,
    #[serde(default)]
    pub resource_expectations: BTreeMap<String, u64>,
}

pub use brood_package_contract::Realm;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Interface {
    pub id: String,
    pub version: u32,
    pub schema: PathBuf,
    pub multiplicity: String,
    pub disconnect: String,
}

impl Extension {
    pub(crate) fn validate(&self, manifest: &Manifest, issues: &mut Vec<ValidationIssue>) {
        if self.schema_version != 1 {
            issue(issues, "extension.schema-version", "expected 1");
        }
        for (field, value) in [
            ("purpose", &self.purpose),
            ("source-revision", &self.source_revision),
            ("state-compatibility", &self.state_compatibility),
            ("migration", &self.migration),
        ] {
            if value.trim().is_empty() {
                issue(issues, format!("extension.{field}"), "must not be empty");
            }
        }
        if !["none", "manual"].contains(&self.migration.as_str()) {
            issue(
                issues,
                "extension.migration",
                "supported migration requirements are none or manual",
            );
        }
        if !(1..=64 * 1024 * 1024).contains(&self.build.log_limit_bytes) {
            issue(
                issues,
                "extension.build.log-limit-bytes",
                "per-stream log limit must be between 1 and 64 MiB",
            );
        }
        if !(1..=86400).contains(&self.build.timeout_seconds) {
            issue(
                issues,
                "extension.build.timeout-seconds",
                "build timeout must be between 1 and 86400 seconds",
            );
        }
        if !self.build.executable.is_absolute()
            || !self.build.executable.starts_with("/nix/store")
            || self
                .build
                .executable
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            issue(
                issues,
                "extension.build.executable",
                "must name an executable in an explicit immutable Nix store environment",
            );
        }
        let mut alternatives = HashSet::new();
        if self.prebuilt.len() > 16 {
            issue(issues, "extension.prebuilt", "alternative limit is 16");
        }
        for alternative in &self.prebuilt {
            if alternative.id.is_empty()
                || alternative.id.len() > 128
                || !alternatives.insert(&alternative.id)
            {
                issue(
                    issues,
                    "extension.prebuilt.id",
                    "expected unique bounded identity",
                );
            }
            if alternative.artifacts.len() != manifest.artifacts.len()
                || manifest
                    .artifacts
                    .iter()
                    .any(|artifact| !alternative.artifacts.contains_key(artifact.id.as_str()))
            {
                issue(
                    issues,
                    "extension.prebuilt.artifacts",
                    "must contain exactly every declared artifact",
                );
            }
            if alternative.host_interfaces.len() > 16
                || alternative.host_interfaces.iter().any(|(id, versions)| {
                    id.is_empty()
                        || id.len() > 128
                        || versions.is_empty()
                        || versions.len() > 16
                        || versions.contains(&0)
                })
            {
                issue(
                    issues,
                    "extension.prebuilt.host-interfaces",
                    "invalid bounded interface requirements",
                );
            }
            for artifact in alternative.artifacts.values() {
                validate_relative_path(issues, "extension.prebuilt.artifacts.path", &artifact.path);
                if artifact.sha256.len() != 64
                    || !artifact
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    issue(
                        issues,
                        "extension.prebuilt.artifacts.sha256",
                        "expected lowercase SHA-256 digest",
                    );
                }
            }
        }
        let mut dependencies = HashSet::new();
        if self.dependencies.len() > 64 {
            issue(issues, "extension.dependencies", "dependency limit is 64");
        }
        for dependency in &self.dependencies {
            validate_relative_path(
                issues,
                "extension.dependencies.manifest",
                &dependency.manifest,
            );
            if dependency.package == manifest.package.id.as_str()
                || !dependencies.insert(&dependency.package)
            {
                issue(
                    issues,
                    "extension.dependencies",
                    "self or duplicate dependency",
                );
            }
        }
        let mut inputs = HashSet::new();
        for path in &self.build.inputs {
            validate_relative_path(issues, "extension.build.inputs", path);
            if !inputs.insert(path) {
                issue(issues, "extension.build.inputs", "duplicate input");
            }
        }
        let schema_ids: HashSet<String> = self
            .interfaces
            .iter()
            .map(|interface| format!("schema:interface:{}", interface.id))
            .chain(
                self.diagnostics
                    .iter()
                    .map(|diagnostic| format!("schema:diagnostic:{}", diagnostic.id)),
            )
            .chain(
                self.configuration_schema
                    .iter()
                    .map(|_| "schema:configuration".to_owned()),
            )
            .collect();
        let document_ids: HashSet<&str> = self
            .documents
            .iter()
            .map(|doc| doc.id.as_str())
            .chain(std::iter::once("manifest"))
            .collect();
        for document in &self.documents {
            if document.references.len() > 64 || document.schemas.len() > 64 {
                issue(
                    issues,
                    "extension.documents",
                    "reference and schema association limit is 64 each",
                );
            }
            for reference in &document.references {
                if !document_ids.contains(reference.as_str()) && !schema_ids.contains(reference) {
                    issue(
                        issues,
                        "extension.documents.references",
                        format!("unknown document reference: {reference}"),
                    );
                }
            }
            for schema in &document.schemas {
                if !schema_ids.contains(schema) {
                    issue(
                        issues,
                        "extension.documents.schemas",
                        format!("unknown schema association: {schema}"),
                    );
                }
            }
        }
        let mut documents = HashSet::new();
        if self.documents.is_empty() {
            issue(
                issues,
                "extension.documents",
                "at least one offline document is required",
            );
        }
        for document in &self.documents {
            if document.id == "manifest" || document.id.starts_with("schema:") {
                issue(
                    issues,
                    "extension.documents.id",
                    "reserved installed contract document ID",
                );
            }

            validate_relative_path(issues, "extension.documents.path", &document.path);
            if document.id.is_empty() || !documents.insert(&document.id) {
                issue(
                    issues,
                    "extension.documents.id",
                    "document IDs must be nonempty and unique",
                );
            }
            if !["reference", "example", "target", "progress", "friction"]
                .contains(&document.category.as_str())
            {
                issue(
                    issues,
                    "extension.documents.category",
                    "unknown knowledge category",
                );
            }
        }
        if self.host_interfaces.len() > 16 {
            issue(
                issues,
                "extension.host-interfaces",
                "host interface limit is 16",
            );
        }
        for (id, versions) in &self.host_interfaces {
            if id.is_empty()
                || id.len() > 128
                || versions.is_empty()
                || versions.len() > 16
                || versions.contains(&0)
                || versions.iter().collect::<HashSet<_>>().len() != versions.len()
            {
                issue(
                    issues,
                    "extension.host-interfaces",
                    "host interfaces require bounded IDs and unique positive acceptable versions",
                );
            }
        }
        let mut diagnostic_ids = HashSet::new();
        if self.diagnostics.len() > 16 {
            issue(issues, "extension.diagnostics", "diagnostic limit is 16");
        }
        for diagnostic in &self.diagnostics {
            if !diagnostic
                .id
                .starts_with(&format!("{}.", manifest.package.id))
                || diagnostic.id.len() > 256
                || !diagnostic_ids.insert(&diagnostic.id)
                || diagnostic.version == 0
            {
                issue(
                    issues,
                    "extension.diagnostics",
                    "diagnostics need unique package-namespaced IDs and positive versions",
                );
            }
            validate_relative_path(issues, "extension.diagnostics.schema", &diagnostic.schema);
        }
        let mut interfaces = HashSet::new();
        for interface in &self.interfaces {
            if interface.id.is_empty()
                || !interfaces.insert(&interface.id)
                || interface.version == 0
            {
                issue(
                    issues,
                    "extension.interfaces",
                    "interfaces require unique IDs and positive versions",
                );
            }
            validate_relative_path(issues, "extension.interfaces.schema", &interface.schema);
            if !["one", "many"].contains(&interface.multiplicity.as_str())
                || !["pending", "stop"].contains(&interface.disconnect.as_str())
            {
                issue(
                    issues,
                    "extension.interfaces",
                    "unsupported multiplicity or disconnect policy",
                );
            }
        }
        if let Some(path) = &self.configuration_schema {
            validate_relative_path(issues, "extension.configuration-schema", path);
        }
        let mut activations = HashSet::new();
        for activation in &self.activation {
            for resource in activation.resource_expectations.keys() {
                if !brood_package_contract::RESOURCE_UNITS
                    .iter()
                    .any(|(id, _)| id == resource)
                {
                    issue(
                        issues,
                        "extension.activation.resource-expectations",
                        "unknown resource expectation; use a resource ID defined by the portable contract",
                    );
                }
            }
            if manifest.artifact(&activation.artifact).is_none()
                || !activations.insert(&activation.artifact)
            {
                issue(
                    issues,
                    "extension.activation.artifact",
                    "must select a unique existing artifact",
                );
            }
            if activation.scope != "per-realm"
                || !["never", "on-failure"].contains(&activation.restart.as_str())
                || activation.deactivation != "stop"
            {
                issue(
                    issues,
                    "extension.activation",
                    "supported lifecycle is per-realm scope, never/on-failure restart, stop deactivation",
                );
            }
            for interface in activation.requires.iter().chain(&activation.provides) {
                if !interfaces.contains(interface) {
                    issue(
                        issues,
                        "extension.activation",
                        format!("unknown interface {interface}"),
                    );
                }
            }
        }
    }
}
