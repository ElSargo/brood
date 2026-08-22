use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, ensure};
use brood_protocol::{IsolationProfile, SessionVolumeLifecycle};
use broodd::session_volume::{
    GRAVEYARD_VOLUME_TTL_SECONDS, RETAINED_VOLUME_TTL_SECONDS,
    SESSION_VOLUME_MANIFEST_SCHEMA_VERSION, SessionVolumeManifest, SessionVolumeStore, VolumeClock,
    VolumeDiscoveryDiagnostic, VolumeDiscoveryDiagnosticKind,
};

#[derive(Clone)]
struct FakeClock(Arc<AtomicU64>);

impl FakeClock {
    fn new(now: u64) -> Self {
        Self(Arc::new(AtomicU64::new(now)))
    }

    fn advance(&self, seconds: u64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }

    fn set(&self, now: u64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl VolumeClock for FakeClock {
    fn now_unix_seconds(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[test]
fn manifest_is_atomic_durable_and_recovered_after_restart() -> Result<()> {
    let directory = tempfile::tempdir().context("create volume store directory")?;
    let clock = FakeClock::new(1_000);
    let mut store = SessionVolumeStore::open_with_clock(directory.path(), clock.clone())?;
    let created = store.create(IsolationProfile::Isolated, "/workspace")?;
    ensure!(created.schema_version == SESSION_VOLUME_MANIFEST_SCHEMA_VERSION);
    ensure!(created.lifecycle == SessionVolumeLifecycle::Active);
    ensure!(store.lookup(created.id) == Some(&created));

    let manifest_path = directory
        .path()
        .join(created.id.to_string())
        .join("manifest.json");
    let persisted: SessionVolumeManifest =
        serde_json::from_slice(&fs::read(&manifest_path)?).context("decode durable manifest")?;
    ensure!(persisted == created);
    ensure!(
        !directory
            .path()
            .join(created.id.to_string())
            .join(".manifest.json.new")
            .exists()
    );

    clock.set(900);
    let reopened = SessionVolumeStore::open_with_clock(directory.path(), clock)?;
    let recovered = reopened.lookup(created.id).context("recover volume")?;
    ensure!(recovered.lifecycle == SessionVolumeLifecycle::Retained);
    ensure!(recovered.retained_at_unix_seconds == Some(1_000));
    ensure!(
        reopened.diagnostics().iter().any(|diagnostic| {
            diagnostic.kind == VolumeDiscoveryDiagnosticKind::RecoveredActive
        })
    );
    Ok(())
}

#[test]
fn fake_clock_proves_retention_and_graveyard_boundaries() -> Result<()> {
    let directory = tempfile::tempdir().context("create volume store directory")?;
    let clock = FakeClock::new(10_000);
    let mut store = SessionVolumeStore::open_with_clock(directory.path(), clock.clone())?;
    let volume = store.create(IsolationProfile::Isolated, "/workspace")?;
    store.transition(volume.id, SessionVolumeLifecycle::Retained)?;

    clock.advance(RETAINED_VOLUME_TTL_SECONDS - 1);
    ensure!(store.run_retention()?.is_empty());
    clock.advance(1);
    let retained = store.run_retention()?;
    ensure!(retained.len() == 1);
    ensure!(retained.first().is_some_and(|transition| {
        transition.from == SessionVolumeLifecycle::Retained
            && transition.to == SessionVolumeLifecycle::Graveyard
    }));

    store.set_pinned(volume.id, true)?;
    clock.advance(GRAVEYARD_VOLUME_TTL_SECONDS);
    ensure!(store.run_retention()?.is_empty());
    store.set_pinned(volume.id, false)?;
    let graveyard = store.run_retention()?;
    ensure!(graveyard.len() == 1);
    ensure!(graveyard.first().is_some_and(|transition| {
        transition.from == SessionVolumeLifecycle::Graveyard
            && transition.to == SessionVolumeLifecycle::Purged
    }));
    ensure!(
        store
            .lookup(volume.id)
            .is_some_and(|manifest| manifest.lifecycle == SessionVolumeLifecycle::Purged)
    );
    Ok(())
}

#[test]
fn discovery_quarantines_invalid_records_without_hiding_valid_ones() -> Result<()> {
    let directory = tempfile::tempdir().context("create volume store directory")?;
    let clock = FakeClock::new(50_000);
    let valid_id = {
        let mut store = SessionVolumeStore::open_with_clock(directory.path(), clock.clone())?;
        let volume = store.create(IsolationProfile::Isolated, "/workspace")?;
        store.transition(volume.id, SessionVolumeLifecycle::Retained)?;
        volume.id
    };
    let invalid = directory.path().join("broken-volume");
    fs::create_dir(&invalid)?;
    fs::write(invalid.join("manifest.json"), b"not-json")?;

    let reopened = SessionVolumeStore::open_with_clock(directory.path(), clock)?;
    ensure!(reopened.lookup(valid_id).is_some());
    ensure!(reopened.list().len() == 1);
    let diagnostic = reopened
        .diagnostics()
        .iter()
        .find(|diagnostic| diagnostic.kind == VolumeDiscoveryDiagnosticKind::DecodeManifest)
        .context("invalid manifest diagnostic")?;
    let quarantined = diagnostic
        .quarantined_to
        .as_ref()
        .context("quarantine destination")?;
    ensure!(quarantined.exists());
    ensure!(!invalid.exists());
    let quarantined_name = quarantined
        .file_name()
        .and_then(|name| name.to_str())
        .context("quarantine file name")?;
    let durable_diagnostic_path = quarantined
        .parent()
        .context("quarantine parent")?
        .join(format!("{quarantined_name}.diagnostic.json"));
    let durable_diagnostic: VolumeDiscoveryDiagnostic =
        serde_json::from_slice(&fs::read(durable_diagnostic_path)?)?;
    ensure!(durable_diagnostic.kind == VolumeDiscoveryDiagnosticKind::DecodeManifest);
    Ok(())
}
