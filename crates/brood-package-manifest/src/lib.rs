//! Typed declarations for assembling and loading Brood plugin packages.
//!
//! A package has one root artifact and zero or more member artifacts. Internal
//! dependency edges determine installation order; placement paths determine
//! the immutable package tree visible to the root and its members.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The manifest schema version implemented by this crate.
pub const SCHEMA_VERSION: u32 = 1;

/// Manifest metadata installed next to the package artifacts.
pub const INSTALLED_MANIFEST_FILE: &str = "brood-package.toml";

/// File containing the root artifact's package-relative placement.
pub const INSTALLED_ROOT_FILE: &str = "brood-package-root";

/// A complete package declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub package: Package,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

/// Package-level identity and root selection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Package {
    /// Stable package identity. Versions and content hashes are separate from identity.
    pub id: PackageId,
    /// Human-facing release version. Package resolution must not infer identity from it.
    pub version: String,
    /// Artifact that receives control when the package is loaded.
    pub root: ArtifactId,
}

/// One runnable artifact installed into a package tree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Artifact {
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    pub abi: ArtifactAbi,
    /// Repository-relative build output consumed by the package assembler.
    pub source: PathBuf,
    /// Package-relative path at which the immutable artifact is installed.
    pub placement: PathBuf,
    /// Host resources this artifact expects to be granted.
    #[serde(default)]
    pub capabilities: Vec<CapabilityId>,
    /// Other artifacts from this package that must be installed first.
    #[serde(default)]
    pub dependencies: Vec<ArtifactId>,
}

/// Runtime-recognized artifact container.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    WasmComponent,
    WasmPreview1,
    WasmCore,
}

/// Brood guest ABI implemented by an artifact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactAbi {
    BroodComponentV1,
    BroodPreview1V1,
    BroodLegacyV1,
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

string_id!(PackageId);
string_id!(ArtifactId);
string_id!(CapabilityId);

/// One independently actionable manifest problem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationIssue {
    pub field: String,
    pub message: String,
}

/// All validation problems found in a parsed manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationErrors {
    issues: Vec<ValidationIssue>,
}

impl ValidationErrors {
    #[must_use]
    pub fn issues(&self) -> &[ValidationIssue] {
        &self.issues
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "package manifest validation failed:")?;
        for issue in &self.issues {
            writeln!(formatter, "- {}: {}", issue.field, issue.message)?;
        }
        Ok(())
    }
}

impl Error for ValidationErrors {}

/// Failure to read, parse, or validate a manifest.
#[derive(Debug)]
pub enum LoadError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(toml::de::Error),
    Validation(ValidationErrors),
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "read {}: {source}", path.display()),
            Self::Parse(source) => write!(formatter, "parse package manifest: {source}"),
            Self::Validation(source) => source.fmt(formatter),
        }
    }
}

impl Error for LoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::Validation(source) => Some(source),
        }
    }
}

impl Manifest {
    /// Parse and validate a TOML manifest.
    pub fn from_toml(source: &str) -> Result<Self, LoadError> {
        let manifest: Self = toml::from_str(source).map_err(LoadError::Parse)?;
        manifest.validate().map_err(LoadError::Validation)?;
        Ok(manifest)
    }

    /// Read, parse, and validate a TOML manifest.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|source| LoadError::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::from_toml(&source)
    }

    /// Validate identities, paths, ABI compatibility, and the internal graph.
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();
        if self.schema_version != SCHEMA_VERSION {
            issue(
                &mut issues,
                "schema-version",
                format!(
                    "unsupported schema version {}; expected {SCHEMA_VERSION}",
                    self.schema_version
                ),
            );
        }
        validate_id(
            &mut issues,
            "package.id",
            self.package.id.as_str(),
            IdSyntax::Package,
        );
        if self.package.version.trim().is_empty() {
            issue(&mut issues, "package.version", "must not be empty");
        }
        validate_id(
            &mut issues,
            "package.root",
            self.package.root.as_str(),
            IdSyntax::Artifact,
        );
        if self.artifacts.is_empty() {
            issue(&mut issues, "artifacts", "must contain a root artifact");
        }

        let mut artifacts = HashMap::new();
        let mut placements = HashMap::new();
        for (index, artifact) in self.artifacts.iter().enumerate() {
            let base = format!("artifacts[{index}]");
            validate_id(
                &mut issues,
                &format!("{base}.id"),
                artifact.id.as_str(),
                IdSyntax::Artifact,
            );
            if let Some(previous) = artifacts.insert(artifact.id.as_str(), index) {
                issue(
                    &mut issues,
                    format!("{base}.id"),
                    format!("duplicates artifacts[{previous}].id"),
                );
            }
            validate_relative_path(&mut issues, &format!("{base}.source"), &artifact.source);
            validate_relative_path(
                &mut issues,
                &format!("{base}.placement"),
                &artifact.placement,
            );
            if let Some(previous) = placements.insert(&artifact.placement, index) {
                issue(
                    &mut issues,
                    format!("{base}.placement"),
                    format!("duplicates artifacts[{previous}].placement"),
                );
            }
            validate_abi(&mut issues, &base, artifact.kind, artifact.abi);

            let mut capabilities = HashSet::new();
            for (capability_index, capability) in artifact.capabilities.iter().enumerate() {
                let field = format!("{base}.capabilities[{capability_index}]");
                validate_id(
                    &mut issues,
                    &field,
                    capability.as_str(),
                    IdSyntax::Capability,
                );
                if !capabilities.insert(capability) {
                    issue(&mut issues, field, "duplicates an earlier capability");
                }
            }
        }

        if !artifacts.contains_key(self.package.root.as_str()) {
            issue(
                &mut issues,
                "package.root",
                format!("unknown artifact `{}`", self.package.root),
            );
        }

        for (index, artifact) in self.artifacts.iter().enumerate() {
            let mut dependencies = HashSet::new();
            for (dependency_index, dependency) in artifact.dependencies.iter().enumerate() {
                let field = format!("artifacts[{index}].dependencies[{dependency_index}]");
                validate_id(&mut issues, &field, dependency.as_str(), IdSyntax::Artifact);
                if dependency == &artifact.id {
                    issue(&mut issues, &field, "must not refer to the artifact itself");
                } else if !artifacts.contains_key(dependency.as_str()) {
                    issue(
                        &mut issues,
                        &field,
                        format!("unknown artifact `{dependency}`"),
                    );
                }
                if !dependencies.insert(dependency) {
                    issue(&mut issues, field, "duplicates an earlier dependency");
                }
            }
        }

        if let Some(cycle) = dependency_cycle(&self.artifacts, &artifacts) {
            issue(
                &mut issues,
                "artifacts.dependencies",
                format!("dependency cycle includes: {}", cycle.join(", ")),
            );
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors { issues })
        }
    }

    /// Find a declared artifact by stable ID.
    #[must_use]
    pub fn artifact(&self, id: &str) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.id.as_str() == id)
    }

    /// Return the package entry artifact after validating the manifest.
    pub fn root_artifact(&self) -> Result<&Artifact, ValidationErrors> {
        self.validate()?;
        // Validation above proves that the root exists.
        self.artifact(self.package.root.as_str())
            .ok_or_else(|| ValidationErrors {
                issues: vec![ValidationIssue {
                    field: "package.root".to_owned(),
                    message: "root artifact disappeared after validation".to_owned(),
                }],
            })
    }

    /// Return a deterministic dependency-first installation order.
    pub fn installation_order(&self) -> Result<Vec<&Artifact>, ValidationErrors> {
        self.validate()?;
        let mut installed = HashSet::new();
        let mut ordered = Vec::with_capacity(self.artifacts.len());

        while ordered.len() < self.artifacts.len() {
            let mut progressed = false;
            for artifact in &self.artifacts {
                if installed.contains(artifact.id.as_str()) {
                    continue;
                }
                if artifact
                    .dependencies
                    .iter()
                    .all(|dependency| installed.contains(dependency.as_str()))
                {
                    installed.insert(artifact.id.as_str());
                    ordered.push(artifact);
                    progressed = true;
                }
            }
            if !progressed {
                return Err(ValidationErrors {
                    issues: vec![ValidationIssue {
                        field: "artifacts.dependencies".to_owned(),
                        message: "dependency graph could not be ordered".to_owned(),
                    }],
                });
            }
        }

        Ok(ordered)
    }
}

#[derive(Clone, Copy)]
enum IdSyntax {
    Package,
    Artifact,
    Capability,
}

fn issue(issues: &mut Vec<ValidationIssue>, field: impl Into<String>, message: impl Into<String>) {
    issues.push(ValidationIssue {
        field: field.into(),
        message: message.into(),
    });
}

fn validate_id(issues: &mut Vec<ValidationIssue>, field: &str, value: &str, syntax: IdSyntax) {
    let mut characters = value.chars();
    let starts_correctly = characters
        .next()
        .is_some_and(|character| character.is_ascii_lowercase());
    let allowed = |character: char| match syntax {
        IdSyntax::Package => {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '.')
        }
        IdSyntax::Artifact => {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        }
        IdSyntax::Capability => {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '.' | ':' | '/')
        }
    };
    if !starts_correctly || !value.chars().all(allowed) {
        issue(
            issues,
            field,
            "must start with a lowercase ASCII letter and contain only portable identifier characters",
        );
    }
}

fn validate_relative_path(issues: &mut Vec<ValidationIssue>, field: &str, path: &Path) {
    let valid = !path.as_os_str().is_empty()
        && !path.is_absolute()
        && !path.to_string_lossy().contains('\\')
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if !valid {
        issue(
            issues,
            field,
            "must be a non-empty portable relative path without `.` or `..` components",
        );
    } else if matches!(
        path.to_str(),
        Some(INSTALLED_MANIFEST_FILE | INSTALLED_ROOT_FILE)
    ) {
        issue(
            issues,
            field,
            "must not collide with installed package metadata",
        );
    }
}

fn validate_abi(
    issues: &mut Vec<ValidationIssue>,
    base: &str,
    kind: ArtifactKind,
    abi: ArtifactAbi,
) {
    let compatible = matches!(
        (kind, abi),
        (ArtifactKind::WasmComponent, ArtifactAbi::BroodComponentV1)
            | (ArtifactKind::WasmPreview1, ArtifactAbi::BroodPreview1V1)
            | (ArtifactKind::WasmCore, ArtifactAbi::BroodLegacyV1)
    );
    if !compatible {
        issue(
            issues,
            format!("{base}.abi"),
            format!("{abi:?} is incompatible with artifact kind {kind:?}"),
        );
    }
}

fn dependency_cycle(artifacts: &[Artifact], indices: &HashMap<&str, usize>) -> Option<Vec<String>> {
    let mut installed = HashSet::new();
    loop {
        let before = installed.len();
        for artifact in artifacts {
            if artifact.dependencies.iter().all(|dependency| {
                !indices.contains_key(dependency.as_str())
                    || installed.contains(dependency.as_str())
            }) {
                installed.insert(artifact.id.as_str());
            }
        }
        if installed.len() == artifacts.len() {
            return None;
        }
        if installed.len() == before {
            return Some(
                artifacts
                    .iter()
                    .filter(|artifact| !installed.contains(artifact.id.as_str()))
                    .map(|artifact| artifact.id.to_string())
                    .collect(),
            );
        }
    }
}
