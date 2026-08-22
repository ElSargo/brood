//! Durable daemon-owned records for session volumes.
//!
//! This module owns manifest persistence and lifecycle policy. It deliberately
//! does not mount volumes or attach them to today's direct sessions.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use brood_protocol::{IsolationProfile, SessionVolumeId, SessionVolumeLifecycle};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SESSION_VOLUME_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const RETAINED_VOLUME_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const GRAVEYARD_VOLUME_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

const MANIFEST_FILE_NAME: &str = "manifest.json";
const QUARANTINE_DIRECTORY_NAME: &str = "quarantine";
const ID_ALLOCATION_ATTEMPTS: usize = 16;

/// Time source used by storage policy.
pub trait VolumeClock {
    fn now_unix_seconds(&self) -> u64;
}

/// Wall-clock source used by the product daemon.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemVolumeClock;

impl VolumeClock for SystemVolumeClock {
    fn now_unix_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs())
    }
}

/// Versioned durable metadata for one session volume.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionVolumeManifest {
    pub schema_version: u32,
    pub id: SessionVolumeId,
    pub created_at_unix_seconds: u64,
    pub last_used_at_unix_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_at_unix_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graveyarded_at_unix_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purged_at_unix_seconds: Option<u64>,
    pub lifecycle: SessionVolumeLifecycle,
    #[serde(default)]
    pub pinned: bool,
    pub isolation_profile: IsolationProfile,
    pub working_directory: String,
}

impl SessionVolumeManifest {
    #[must_use]
    pub const fn retention_eligible_at(&self) -> Option<u64> {
        if self.pinned {
            return None;
        }
        match self.lifecycle {
            SessionVolumeLifecycle::Retained => Some(saturating_deadline(
                match self.retained_at_unix_seconds {
                    Some(retained) => retained,
                    None => self.last_used_at_unix_seconds,
                },
                RETAINED_VOLUME_TTL_SECONDS,
            )),
            SessionVolumeLifecycle::Graveyard => match self.graveyarded_at_unix_seconds {
                Some(graveyarded) => Some(saturating_deadline(
                    graveyarded,
                    GRAVEYARD_VOLUME_TTL_SECONDS,
                )),
                None => None,
            },
            SessionVolumeLifecycle::Active | SessionVolumeLifecycle::Purged => None,
        }
    }
}

const fn saturating_deadline(timestamp: u64, duration: u64) -> u64 {
    timestamp.saturating_add(duration)
}

/// One lifecycle change committed by retention maintenance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumeLifecycleTransition {
    pub id: SessionVolumeId,
    pub from: SessionVolumeLifecycle,
    pub to: SessionVolumeLifecycle,
    pub at_unix_seconds: u64,
}

/// Classification of a startup discovery observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VolumeDiscoveryDiagnosticKind {
    ReadDirectory,
    UnexpectedEntry,
    MissingManifest,
    ReadManifest,
    DecodeManifest,
    UnsupportedSchema,
    MismatchedIdentity,
    InvalidManifest,
    DuplicateIdentity,
    QuarantineFailed,
    RecoveredActive,
}

/// Structured startup evidence retained for operator diagnostics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VolumeDiscoveryDiagnostic {
    pub kind: VolumeDiscoveryDiagnosticKind,
    pub path: PathBuf,
    pub message: String,
    pub quarantined_to: Option<PathBuf>,
}

/// Failure to read or durably mutate the volume store.
#[derive(Debug, Error)]
pub enum SessionVolumeStoreError {
    #[error("{operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("secure random session volume identity is unavailable: {0}")]
    IdentityUnavailable(String),
    #[error("session volume {0} is unknown")]
    Unknown(SessionVolumeId),
    #[error("session volume {id} cannot transition from {from:?} to {to:?}")]
    InvalidTransition {
        id: SessionVolumeId,
        from: SessionVolumeLifecycle,
        to: SessionVolumeLifecycle,
    },
    #[error("session volume {0} has already been purged")]
    Purged(SessionVolumeId),
    #[error("session volume manifest is invalid: {0}")]
    InvalidManifest(String),
}

/// Daemon-owned durable session volume catalogue.
pub struct SessionVolumeStore<C = SystemVolumeClock> {
    root: PathBuf,
    clock: C,
    manifests: BTreeMap<SessionVolumeId, SessionVolumeManifest>,
    diagnostics: Vec<VolumeDiscoveryDiagnostic>,
}

impl SessionVolumeStore<SystemVolumeClock> {
    /// Discover an existing store using the system wall clock.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, SessionVolumeStoreError> {
        Self::open_with_clock(root, SystemVolumeClock)
    }
}

impl<C: VolumeClock> SessionVolumeStore<C> {
    /// Discover an existing store using an injected policy clock.
    pub fn open_with_clock(
        root: impl Into<PathBuf>,
        clock: C,
    ) -> Result<Self, SessionVolumeStoreError> {
        let root = root.into();
        create_directory(&root)?;
        create_directory(&root.join(QUARANTINE_DIRECTORY_NAME))?;
        let mut store = Self {
            root,
            clock,
            manifests: BTreeMap::new(),
            diagnostics: Vec::new(),
        };
        store.discover()?;
        store.recover_active_records()?;
        Ok(store)
    }

    /// Filesystem root containing volume directories and quarantine.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read-only ordered view of all discovered volume manifests.
    pub fn list(&self) -> impl ExactSizeIterator<Item = &SessionVolumeManifest> {
        self.manifests.values()
    }

    /// Look up one stable volume identity.
    #[must_use]
    pub fn lookup(&self, id: SessionVolumeId) -> Option<&SessionVolumeManifest> {
        self.manifests.get(&id)
    }

    /// Startup recovery and quarantine observations.
    #[must_use]
    pub fn diagnostics(&self) -> &[VolumeDiscoveryDiagnostic] {
        &self.diagnostics
    }

    /// Create and durably publish a new active volume manifest.
    pub fn create(
        &mut self,
        isolation_profile: IsolationProfile,
        working_directory: impl Into<String>,
    ) -> Result<SessionVolumeManifest, SessionVolumeStoreError> {
        let working_directory = working_directory.into();
        if working_directory.is_empty() {
            return Err(SessionVolumeStoreError::InvalidManifest(
                "working directory is empty".to_string(),
            ));
        }
        let id = self.allocate_id()?;
        let now = self.clock.now_unix_seconds();
        let manifest = SessionVolumeManifest {
            schema_version: SESSION_VOLUME_MANIFEST_SCHEMA_VERSION,
            id,
            created_at_unix_seconds: now,
            last_used_at_unix_seconds: now,
            retained_at_unix_seconds: None,
            graveyarded_at_unix_seconds: None,
            purged_at_unix_seconds: None,
            lifecycle: SessionVolumeLifecycle::Active,
            pinned: false,
            isolation_profile,
            working_directory,
        };
        validate_manifest(&manifest)?;
        let directory = self.volume_directory(id);
        create_directory(&directory)?;
        if let Err(error) = synchronize_directory(&self.root) {
            let _cleanup_result = fs::remove_dir(&directory);
            return Err(error);
        }
        if let Err(error) = persist_manifest(&directory, &manifest) {
            let _cleanup_result = fs::remove_dir(&directory);
            return Err(error);
        }
        self.manifests.insert(id, manifest.clone());
        Ok(manifest)
    }

    /// Record use without changing lifecycle.
    pub fn mark_used(
        &mut self,
        id: SessionVolumeId,
    ) -> Result<SessionVolumeManifest, SessionVolumeStoreError> {
        self.update(id, |manifest, now| {
            if manifest.lifecycle == SessionVolumeLifecycle::Purged {
                return Err(SessionVolumeStoreError::Purged(id));
            }
            manifest.last_used_at_unix_seconds = manifest.last_used_at_unix_seconds.max(now);
            Ok(())
        })
    }

    /// Pin or unpin a non-purged volume.
    pub fn set_pinned(
        &mut self,
        id: SessionVolumeId,
        pinned: bool,
    ) -> Result<SessionVolumeManifest, SessionVolumeStoreError> {
        self.update(id, |manifest, _now| {
            if manifest.lifecycle == SessionVolumeLifecycle::Purged {
                return Err(SessionVolumeStoreError::Purged(id));
            }
            manifest.pinned = pinned;
            Ok(())
        })
    }

    /// Apply one explicit valid lifecycle transition and persist it atomically.
    pub fn transition(
        &mut self,
        id: SessionVolumeId,
        to: SessionVolumeLifecycle,
    ) -> Result<SessionVolumeManifest, SessionVolumeStoreError> {
        self.update(id, |manifest, now| transition_manifest(manifest, to, now))
    }

    /// Advance every eligible unpinned record by one retention stage.
    pub fn run_retention(
        &mut self,
    ) -> Result<Vec<VolumeLifecycleTransition>, SessionVolumeStoreError> {
        let now = self.clock.now_unix_seconds();
        let eligible = self
            .manifests
            .values()
            .filter_map(|manifest| {
                manifest
                    .retention_eligible_at()
                    .filter(|deadline| now >= *deadline)
                    .map(|_| (manifest.id, manifest.lifecycle))
            })
            .collect::<Vec<_>>();
        let mut transitions = Vec::with_capacity(eligible.len());
        for (id, from) in eligible {
            let to = match from {
                SessionVolumeLifecycle::Retained => SessionVolumeLifecycle::Graveyard,
                SessionVolumeLifecycle::Graveyard => SessionVolumeLifecycle::Purged,
                SessionVolumeLifecycle::Active | SessionVolumeLifecycle::Purged => continue,
            };
            self.update_at(id, now, |manifest, at| {
                transition_manifest(manifest, to, at)
            })?;
            transitions.push(VolumeLifecycleTransition {
                id,
                from,
                to,
                at_unix_seconds: now,
            });
        }
        Ok(transitions)
    }

    fn allocate_id(&self) -> Result<SessionVolumeId, SessionVolumeStoreError> {
        for _ in 0..ID_ALLOCATION_ATTEMPTS {
            let mut bytes = [0_u8; 16];
            getrandom::fill(&mut bytes)
                .map_err(|error| SessionVolumeStoreError::IdentityUnavailable(error.to_string()))?;
            let id = SessionVolumeId::from_bytes(bytes);
            if bytes != [0_u8; 16]
                && !self.manifests.contains_key(&id)
                && !self.volume_directory(id).exists()
            {
                return Ok(id);
            }
        }
        Err(SessionVolumeStoreError::IdentityUnavailable(
            "allocation attempts exhausted".to_string(),
        ))
    }

    fn update<F>(
        &mut self,
        id: SessionVolumeId,
        update: F,
    ) -> Result<SessionVolumeManifest, SessionVolumeStoreError>
    where
        F: FnOnce(&mut SessionVolumeManifest, u64) -> Result<(), SessionVolumeStoreError>,
    {
        self.update_at(id, self.clock.now_unix_seconds(), update)
    }

    fn update_at<F>(
        &mut self,
        id: SessionVolumeId,
        now: u64,
        update: F,
    ) -> Result<SessionVolumeManifest, SessionVolumeStoreError>
    where
        F: FnOnce(&mut SessionVolumeManifest, u64) -> Result<(), SessionVolumeStoreError>,
    {
        let mut manifest = self
            .manifests
            .get(&id)
            .cloned()
            .ok_or(SessionVolumeStoreError::Unknown(id))?;
        update(&mut manifest, now)?;
        validate_manifest(&manifest)?;
        persist_manifest(&self.volume_directory(id), &manifest)?;
        self.manifests.insert(id, manifest.clone());
        Ok(manifest)
    }

    fn discover(&mut self) -> Result<(), SessionVolumeStoreError> {
        let entries = fs::read_dir(&self.root).map_err(|source| SessionVolumeStoreError::Io {
            operation: "read session volume store",
            path: self.root.clone(),
            source,
        })?;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.diagnostics.push(VolumeDiscoveryDiagnostic {
                        kind: VolumeDiscoveryDiagnosticKind::ReadDirectory,
                        path: self.root.clone(),
                        message: error.to_string(),
                        quarantined_to: None,
                    });
                    continue;
                }
            };
            let path = entry.path();
            if entry.file_name() == QUARANTINE_DIRECTORY_NAME {
                continue;
            }
            let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if !is_directory {
                self.quarantine(
                    &path,
                    VolumeDiscoveryDiagnosticKind::UnexpectedEntry,
                    "volume store entry is not a directory".to_string(),
                );
                continue;
            }
            self.discover_volume(&path);
        }
        Ok(())
    }

    fn discover_volume(&mut self, directory: &Path) {
        let manifest_path = directory.join(MANIFEST_FILE_NAME);
        let bytes = match fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                let kind = if error.kind() == io::ErrorKind::NotFound {
                    VolumeDiscoveryDiagnosticKind::MissingManifest
                } else {
                    VolumeDiscoveryDiagnosticKind::ReadManifest
                };
                self.quarantine(directory, kind, error.to_string());
                return;
            }
        };
        let manifest = match serde_json::from_slice::<SessionVolumeManifest>(&bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                self.quarantine(
                    directory,
                    VolumeDiscoveryDiagnosticKind::DecodeManifest,
                    error.to_string(),
                );
                return;
            }
        };
        if manifest.schema_version != SESSION_VOLUME_MANIFEST_SCHEMA_VERSION {
            self.quarantine(
                directory,
                VolumeDiscoveryDiagnosticKind::UnsupportedSchema,
                format!("unsupported schema version {}", manifest.schema_version),
            );
            return;
        }
        let directory_identity = directory.file_name().and_then(|name| name.to_str());
        if directory_identity != Some(manifest.id.to_string().as_str()) {
            self.quarantine(
                directory,
                VolumeDiscoveryDiagnosticKind::MismatchedIdentity,
                "manifest identity does not match its directory".to_string(),
            );
            return;
        }
        if let Err(error) = validate_manifest(&manifest) {
            self.quarantine(
                directory,
                VolumeDiscoveryDiagnosticKind::InvalidManifest,
                error.to_string(),
            );
            return;
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = self.manifests.entry(manifest.id)
        {
            entry.insert(manifest);
            return;
        }
        self.quarantine(
            directory,
            VolumeDiscoveryDiagnosticKind::DuplicateIdentity,
            "stable volume identity was already discovered".to_string(),
        );
    }

    fn recover_active_records(&mut self) -> Result<(), SessionVolumeStoreError> {
        let active = self
            .manifests
            .values()
            .filter(|manifest| manifest.lifecycle == SessionVolumeLifecycle::Active)
            .map(|manifest| manifest.id)
            .collect::<Vec<_>>();
        for id in active {
            let recovered = self.transition(id, SessionVolumeLifecycle::Retained)?;
            self.diagnostics.push(VolumeDiscoveryDiagnostic {
                kind: VolumeDiscoveryDiagnosticKind::RecoveredActive,
                path: self.volume_directory(id),
                message: format!(
                    "recovered stale active volume as retained at {}",
                    recovered.retained_at_unix_seconds.unwrap_or_default()
                ),
                quarantined_to: None,
            });
        }
        Ok(())
    }

    fn quarantine(&mut self, path: &Path, kind: VolumeDiscoveryDiagnosticKind, message: String) {
        let Some(destination) = self.quarantine_destination(path) else {
            self.diagnostics.push(VolumeDiscoveryDiagnostic {
                kind: VolumeDiscoveryDiagnosticKind::QuarantineFailed,
                path: path.to_path_buf(),
                message: format!("{message}; quarantine destination space is exhausted"),
                quarantined_to: None,
            });
            return;
        };
        match fs::rename(path, &destination) {
            Ok(()) => {
                let mut diagnostic = VolumeDiscoveryDiagnostic {
                    kind,
                    path: path.to_path_buf(),
                    message,
                    quarantined_to: Some(destination.clone()),
                };
                let quarantine_directory = self.root.join(QUARANTINE_DIRECTORY_NAME);
                for (label, directory) in [
                    ("volume store", self.root.as_path()),
                    ("quarantine directory", quarantine_directory.as_path()),
                ] {
                    if let Err(error) = synchronize_directory(directory) {
                        diagnostic.message = format!(
                            "{}; failed to synchronize {label} after quarantine: {error}",
                            diagnostic.message
                        );
                    }
                }
                if let Err(error) = persist_quarantine_diagnostic(
                    &self.root.join(QUARANTINE_DIRECTORY_NAME),
                    &destination,
                    &diagnostic,
                ) {
                    diagnostic.message = format!(
                        "{}; failed to persist quarantine diagnostic: {error}",
                        diagnostic.message
                    );
                }
                self.diagnostics.push(diagnostic);
            }
            Err(error) => self.diagnostics.push(VolumeDiscoveryDiagnostic {
                kind: VolumeDiscoveryDiagnosticKind::QuarantineFailed,
                path: path.to_path_buf(),
                message: format!("{message}; quarantine failed: {error}"),
                quarantined_to: None,
            }),
        }
    }

    fn quarantine_destination(&self, path: &Path) -> Option<PathBuf> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown");
        let now = self.clock.now_unix_seconds();
        let quarantine = self.root.join(QUARANTINE_DIRECTORY_NAME);
        for suffix in 0_u16..=u16::MAX {
            let destination = quarantine.join(format!("{name}-{now}-{suffix}"));
            if !destination.exists() {
                return Some(destination);
            }
        }
        None
    }

    fn volume_directory(&self, id: SessionVolumeId) -> PathBuf {
        self.root.join(id.to_string())
    }
}

const fn transition_manifest(
    manifest: &mut SessionVolumeManifest,
    to: SessionVolumeLifecycle,
    requested_at: u64,
) -> Result<(), SessionVolumeStoreError> {
    let from = manifest.lifecycle;
    let valid = matches!(
        (from, to),
        (
            SessionVolumeLifecycle::Active,
            SessionVolumeLifecycle::Retained | SessionVolumeLifecycle::Graveyard
        ) | (
            SessionVolumeLifecycle::Retained,
            SessionVolumeLifecycle::Active | SessionVolumeLifecycle::Graveyard
        ) | (
            SessionVolumeLifecycle::Graveyard,
            SessionVolumeLifecycle::Retained | SessionVolumeLifecycle::Purged
        )
    );
    if !valid {
        return Err(SessionVolumeStoreError::InvalidTransition {
            id: manifest.id,
            from,
            to,
        });
    }
    let mut now = requested_at;
    if now < manifest.created_at_unix_seconds {
        now = manifest.created_at_unix_seconds;
    }
    if now < manifest.last_used_at_unix_seconds {
        now = manifest.last_used_at_unix_seconds;
    }
    if let Some(retained) = manifest.retained_at_unix_seconds
        && now < retained
    {
        now = retained;
    }
    if let Some(graveyarded) = manifest.graveyarded_at_unix_seconds
        && now < graveyarded
    {
        now = graveyarded;
    }
    manifest.lifecycle = to;
    match to {
        SessionVolumeLifecycle::Active => {
            manifest.last_used_at_unix_seconds = now;
            manifest.retained_at_unix_seconds = None;
            manifest.graveyarded_at_unix_seconds = None;
            manifest.purged_at_unix_seconds = None;
        }
        SessionVolumeLifecycle::Retained => {
            manifest.retained_at_unix_seconds = Some(now);
            manifest.graveyarded_at_unix_seconds = None;
            manifest.purged_at_unix_seconds = None;
        }
        SessionVolumeLifecycle::Graveyard => {
            manifest.graveyarded_at_unix_seconds = Some(now);
            manifest.purged_at_unix_seconds = None;
        }
        SessionVolumeLifecycle::Purged => {
            manifest.purged_at_unix_seconds = Some(now);
        }
    }
    Ok(())
}

fn validate_manifest(manifest: &SessionVolumeManifest) -> Result<(), SessionVolumeStoreError> {
    if manifest.schema_version != SESSION_VOLUME_MANIFEST_SCHEMA_VERSION {
        return Err(SessionVolumeStoreError::InvalidManifest(format!(
            "unsupported schema version {}",
            manifest.schema_version
        )));
    }
    if *manifest.id.as_bytes() == [0_u8; 16] {
        return Err(SessionVolumeStoreError::InvalidManifest(
            "stable volume identity is zero".to_string(),
        ));
    }
    if manifest.working_directory.is_empty() {
        return Err(SessionVolumeStoreError::InvalidManifest(
            "working directory is empty".to_string(),
        ));
    }
    if manifest.last_used_at_unix_seconds < manifest.created_at_unix_seconds {
        return Err(SessionVolumeStoreError::InvalidManifest(
            "last-use timestamp precedes creation".to_string(),
        ));
    }
    for (name, timestamp) in [
        ("retention", manifest.retained_at_unix_seconds),
        ("graveyard", manifest.graveyarded_at_unix_seconds),
        ("purge", manifest.purged_at_unix_seconds),
    ] {
        if timestamp.is_some_and(|timestamp| timestamp < manifest.created_at_unix_seconds) {
            return Err(SessionVolumeStoreError::InvalidManifest(format!(
                "{name} timestamp precedes creation"
            )));
        }
    }
    if manifest
        .graveyarded_at_unix_seconds
        .zip(manifest.retained_at_unix_seconds)
        .is_some_and(|(graveyarded, retained)| graveyarded < retained)
    {
        return Err(SessionVolumeStoreError::InvalidManifest(
            "graveyard timestamp precedes retention".to_string(),
        ));
    }
    if manifest
        .purged_at_unix_seconds
        .zip(manifest.graveyarded_at_unix_seconds)
        .is_some_and(|(purged, graveyarded)| purged < graveyarded)
    {
        return Err(SessionVolumeStoreError::InvalidManifest(
            "purge timestamp precedes graveyard".to_string(),
        ));
    }
    let lifecycle_timestamp_present = match manifest.lifecycle {
        SessionVolumeLifecycle::Active => true,
        SessionVolumeLifecycle::Retained => manifest.retained_at_unix_seconds.is_some(),
        SessionVolumeLifecycle::Graveyard => manifest.graveyarded_at_unix_seconds.is_some(),
        SessionVolumeLifecycle::Purged => manifest.purged_at_unix_seconds.is_some(),
    };
    if !lifecycle_timestamp_present {
        return Err(SessionVolumeStoreError::InvalidManifest(
            "lifecycle timestamp is missing".to_string(),
        ));
    }
    Ok(())
}

fn create_directory(path: &Path) -> Result<(), SessionVolumeStoreError> {
    fs::create_dir_all(path).map_err(|source| SessionVolumeStoreError::Io {
        operation: "create directory",
        path: path.to_path_buf(),
        source,
    })
}

fn synchronize_directory(path: &Path) -> Result<(), SessionVolumeStoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| SessionVolumeStoreError::Io {
            operation: "synchronize directory",
            path: path.to_path_buf(),
            source,
        })
}

fn persist_manifest(
    directory: &Path,
    manifest: &SessionVolumeManifest,
) -> Result<(), SessionVolumeStoreError> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        SessionVolumeStoreError::InvalidManifest(format!("encode manifest: {error}"))
    })?;
    persist_bytes(directory, MANIFEST_FILE_NAME, &bytes)
}

fn persist_quarantine_diagnostic(
    quarantine_directory: &Path,
    destination: &Path,
    diagnostic: &VolumeDiscoveryDiagnostic,
) -> Result<(), SessionVolumeStoreError> {
    let destination_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            SessionVolumeStoreError::InvalidManifest(
                "quarantine destination has no UTF-8 file name".to_string(),
            )
        })?;
    let file_name = format!("{destination_name}.diagnostic.json");
    let bytes = serde_json::to_vec_pretty(diagnostic).map_err(|error| {
        SessionVolumeStoreError::InvalidManifest(format!("encode quarantine diagnostic: {error}"))
    })?;
    persist_bytes(quarantine_directory, &file_name, &bytes)
}

fn persist_bytes(
    directory: &Path,
    file_name: &str,
    bytes: &[u8],
) -> Result<(), SessionVolumeStoreError> {
    let destination = directory.join(file_name);
    let temporary = directory.join(format!(".{file_name}.new"));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|source| SessionVolumeStoreError::Io {
                operation: "create temporary record",
                path: temporary.clone(),
                source,
            })?;
        file.write_all(bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|source| SessionVolumeStoreError::Io {
                operation: "write temporary record",
                path: temporary.clone(),
                source,
            })?;
        fs::rename(&temporary, &destination).map_err(|source| SessionVolumeStoreError::Io {
            operation: "replace durable record",
            path: destination.clone(),
            source,
        })?;
        synchronize_directory(directory)
    })();
    if result.is_err() {
        let _cleanup_result = fs::remove_file(temporary);
    }
    result
}
