//! Durable, presentation-owned complaint capture.
//!
//! This module deliberately contains no backend filesystem assumptions.  A
//! presentation may be connected to a remote backend, while the complaint
//! bundle always belongs to the machine displaying Brood.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u32 = 1;
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CollectionStatus {
    Pending,
    Available,
    Omitted,
    Unavailable,
    Failed,
    Truncated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InvestigationStatus {
    NotRequested,
    Unavailable,
    Queued,
    Running,
    Complete,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CaptureResult {
    pub status: CollectionStatus,
    pub collected_at_unix_ms: Option<u64>,
    pub frame_sequence: Option<u64>,
    pub path: Option<String>,
    pub detail: Option<String>,
}

impl CaptureResult {
    fn pending(path: &str, frame_sequence: Option<u64>) -> Self {
        Self {
            status: CollectionStatus::Pending,
            collected_at_unix_ms: None,
            frame_sequence,
            path: Some(path.to_owned()),
            detail: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Origin {
    pub client_id: String,
    pub backend_id: Option<u64>,
    pub session_id: Option<u64>,
    pub focused_pane: Option<u64>,
    pub visible_panes: Vec<u64>,
    pub frame_sequence: Option<u64>,
    pub layout_revision: u64,
    pub presentation_width: u32,
    pub presentation_height: u32,
    pub scale_milli: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Investigation {
    pub id: Option<String>,
    pub status: InvestigationStatus,
    pub updated_at_unix_ms: u64,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub id: String,
    pub created_at_unix_ms: u64,
    pub saved_at_unix_ms: Option<u64>,
    pub origin: Origin,
    pub screenshot: CaptureResult,
    pub diagnostics: CaptureResult,
    pub investigation: Investigation,
}

#[derive(Clone, Debug)]
pub struct Draft {
    pub manifest: Manifest,
    pub note: String,
    pub include_screenshot: bool,
    pub include_diagnostics: bool,
}

#[derive(Debug)]
pub struct ComplaintStore {
    root: PathBuf,
    draft: Option<Draft>,
}

impl Default for ComplaintStore {
    fn default() -> Self {
        Self::new(Self::default_root())
    }
}

impl ComplaintStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root, draft: None }
    }

    pub fn default_root() -> PathBuf {
        if let Some(root) = std::env::var_os("BROOD_COMPLAINTS_ROOT") {
            return PathBuf::from(root);
        }
        let data = std::env::var_os("XDG_DATA_HOME").map_or_else(
            || {
                std::env::var_os("HOME").map_or_else(
                    || PathBuf::from(".local/share"),
                    |home| PathBuf::from(home).join(".local/share"),
                )
            },
            PathBuf::from,
        );
        data.join("brood/complaints")
    }

    pub fn open(&mut self, origin: Origin, client_id: &str) -> &Draft {
        if self.draft.is_none() {
            self.draft = Some(new_draft(origin, client_id));
        }
        self.draft.as_ref().expect("draft was just initialized")
    }

    pub fn fresh(&mut self, origin: Origin, client_id: &str) -> &Draft {
        self.draft = Some(new_draft(origin, client_id));
        self.draft.as_ref().expect("draft was just initialized")
    }

    pub fn draft(&self) -> Option<&Draft> {
        self.draft.as_ref()
    }
    pub fn draft_mut(&mut self) -> Option<&mut Draft> {
        self.draft.as_mut()
    }
    pub fn close_editor(&mut self) { /* The capture and words intentionally remain latched. */
    }

    pub fn save(&mut self, investigate: bool) -> Result<PathBuf> {
        let draft = self.draft.as_mut().context("no complaint draft is open")?;
        ensure!(!draft.note.trim().is_empty(), "complaint note is empty");
        let directory = self.root.join(&draft.manifest.id);
        fs::create_dir_all(directory.join("evidence"))
            .with_context(|| format!("create complaint directory {}", directory.display()))?;

        if !draft.include_screenshot {
            draft.manifest.screenshot.status = CollectionStatus::Omitted;
            draft.manifest.screenshot.path = None;
        }
        if !draft.include_diagnostics {
            draft.manifest.diagnostics.status = CollectionStatus::Omitted;
            draft.manifest.diagnostics.path = None;
        }
        let now = unix_ms();
        draft.manifest.saved_at_unix_ms = Some(now);
        if investigate && draft.manifest.investigation.status == InvestigationStatus::NotRequested {
            // Agent launch is intentionally separate from durable note success.
            draft.manifest.investigation.status = InvestigationStatus::Unavailable;
            draft.manifest.investigation.detail = Some(
                "agent investigation transport is not implemented yet; complaint was saved".into(),
            );
            draft.manifest.investigation.updated_at_unix_ms = now;
        }
        write_atomic(&directory.join("complaint.md"), draft.note.as_bytes())?;
        let manifest = serde_json::to_vec_pretty(&draft.manifest).context("encode manifest")?;
        write_atomic(&directory.join("manifest.json"), &manifest)?;
        sync_directory(&directory)?;
        Ok(directory)
    }

    pub fn record_screenshot(&mut self, png: &[u8]) -> Result<PathBuf> {
        let draft = self.draft.as_mut().context("no complaint draft is open")?;
        ensure!(draft.include_screenshot, "screenshot was omitted");
        ensure!(
            png.starts_with(b"\x89PNG\r\n\x1a\n"),
            "capture is not PNG data"
        );
        let directory = self.root.join(&draft.manifest.id).join("evidence");
        fs::create_dir_all(&directory)?;
        let path = directory.join("presentation.png");
        write_atomic(&path, png)?;
        draft.manifest.screenshot.status = CollectionStatus::Available;
        draft.manifest.screenshot.collected_at_unix_ms = Some(unix_ms());
        Ok(path)
    }

    pub fn mark_screenshot_unavailable(&mut self, detail: impl Into<String>) -> Result<()> {
        let draft = self.draft.as_mut().context("no complaint draft is open")?;
        draft.manifest.screenshot.status = CollectionStatus::Unavailable;
        draft.manifest.screenshot.collected_at_unix_ms = Some(unix_ms());
        draft.manifest.screenshot.path = None;
        draft.manifest.screenshot.detail = Some(detail.into());
        Ok(())
    }

    pub fn record_diagnostics(&mut self, value: &serde_json::Value) -> Result<PathBuf> {
        let draft = self.draft.as_mut().context("no complaint draft is open")?;
        ensure!(draft.include_diagnostics, "diagnostics were omitted");
        let directory = self.root.join(&draft.manifest.id).join("evidence");
        fs::create_dir_all(&directory)?;
        let path = directory.join("presentation.json");
        write_atomic(&path, &serde_json::to_vec_pretty(value)?)?;
        draft.manifest.diagnostics.status = CollectionStatus::Available;
        draft.manifest.diagnostics.collected_at_unix_ms = Some(unix_ms());
        Ok(path)
    }

    pub fn list(&self) -> Result<Vec<Manifest>> {
        let mut manifests = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Ok(manifests);
        };
        for entry in entries {
            let path = entry?.path().join("manifest.json");
            if !path.is_file() {
                continue;
            }
            let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            manifests.push(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("decode {}", path.display()))?,
            );
        }
        manifests.sort_by_key(|manifest: &Manifest| std::cmp::Reverse(manifest.created_at_unix_ms));
        Ok(manifests)
    }
}

fn new_draft(mut origin: Origin, client_id: &str) -> Draft {
    origin.client_id = client_id.to_owned();
    let now = unix_ms();
    let id = format!(
        "{now:013x}-{:08x}-{:04x}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    );
    Draft {
        manifest: Manifest {
            schema_version: SCHEMA_VERSION,
            id,
            created_at_unix_ms: now,
            saved_at_unix_ms: None,
            screenshot: CaptureResult::pending("evidence/presentation.png", origin.frame_sequence),
            diagnostics: CaptureResult::pending(
                "evidence/presentation.json",
                origin.frame_sequence,
            ),
            investigation: Investigation {
                id: None,
                status: InvestigationStatus::NotRequested,
                updated_at_unix_ms: now,
                detail: None,
            },
            origin,
        },
        note: String::new(),
        include_screenshot: true,
        include_diagnostics: true,
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid bundle filename")?;
    let temporary = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        drop(fs::remove_file(&temporary));
    }
    result
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .context("sync complaint directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin {
            client_id: String::new(),
            backend_id: Some(2),
            session_id: Some(3),
            focused_pane: Some(4),
            visible_panes: vec![4, 5],
            frame_sequence: Some(9),
            layout_revision: 7,
            presentation_width: 800,
            presentation_height: 600,
            scale_milli: 1000,
        }
    }

    #[test]
    fn close_and_reopen_retains_original_capture_and_words() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = ComplaintStore::new(temp.path().into());
        let id = store.open(origin(), "client-a").manifest.id.clone();
        store.draft_mut().unwrap().note.push_str("the pane flashed");
        store.close_editor();
        assert_eq!(store.open(origin(), "client-b").manifest.id, id);
        assert_eq!(store.draft().unwrap().note, "the pane flashed");
        assert_eq!(store.draft().unwrap().manifest.origin.client_id, "client-a");
    }

    #[test]
    fn save_is_idempotent_and_original_note_is_separate() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = ComplaintStore::new(temp.path().into());
        store.open(origin(), "client-a");
        store.draft_mut().unwrap().note = "rough report\n".into();
        let first = store.save(true).unwrap();
        let second = store.save(true).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs::read_to_string(first.join("complaint.md")).unwrap(),
            "rough report\n"
        );
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(first.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            manifest.investigation.status,
            InvestigationStatus::Unavailable
        );
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn omitted_evidence_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = ComplaintStore::new(temp.path().into());
        store.open(origin(), "client-a");
        let draft = store.draft_mut().unwrap();
        draft.note = "bad focus".into();
        draft.include_screenshot = false;
        draft.include_diagnostics = false;
        let path = store.save(false).unwrap();
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.screenshot.status, CollectionStatus::Omitted);
        assert_eq!(manifest.diagnostics.status, CollectionStatus::Omitted);
    }
}
