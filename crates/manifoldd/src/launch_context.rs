#![allow(
    clippy::let_underscore_must_use,
    clippy::string_slice,
    reason = "tracked in issues.md: launch context logging still truncates display strings by byte offsets"
)]
// TODO(lint-structure): define environment preview truncation in Unicode scalar terms.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

use crate::SessionArgs;

const SCOPE_PREFIX: &str = "manifold-launch-";

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LaunchRecord {
    pub id: String,
    pub project_dir: PathBuf,
    pub working_dir: PathBuf,
    pub cache_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_cache_root: Option<PathBuf>,
    pub runner_kind: String,
    pub program_argv: Vec<OsString>,
    pub session_args: Option<SessionArgs>,
}

impl LaunchRecord {
    #[must_use]
    pub fn env_pairs(&self) -> [(String, OsString); 3] {
        [
            (
                "MANIFOLD_SESSION_PROJECT_DIR".to_string(),
                self.project_dir.as_os_str().to_owned(),
            ),
            (
                "MANIFOLD_SESSION_WORKING_DIR".to_string(),
                self.working_dir.as_os_str().to_owned(),
            ),
            (
                "MANIFOLD_SESSION_CACHE_DIR".to_string(),
                self.cache_dir.as_os_str().to_owned(),
            ),
        ]
    }
}

#[must_use]
pub fn new_launch_id() -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    format!("p{}t{stamp}", std::process::id())
}

#[must_use]
pub fn scope_unit(id: &str) -> String {
    format!("{SCOPE_PREFIX}{id}.scope")
}

pub fn write_launch_record(record: &LaunchRecord) -> Result<PathBuf> {
    let path = record_path(&record.id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let encoded = serde_json::to_vec_pretty(record).context("encode Manifold launch record")?;
    fs::write(&path, encoded).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

pub fn remove_launch_record(id: &str) {
    if let Ok(path) = record_path(id) {
        let _ = fs::remove_file(path);
    }
}

pub fn current_launch_record() -> Result<LaunchRecord> {
    let id = current_launch_id()?;
    read_launch_record(&id)
}

fn read_launch_record(id: &str) -> Result<LaunchRecord> {
    let path = record_path(id)?;
    let contents = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&contents).with_context(|| format!("decode {}", path.display()))
}

fn current_launch_id() -> Result<String> {
    let cgroup = fs::read_to_string("/proc/self/cgroup").context("read /proc/self/cgroup")?;
    cgroup
        .lines()
        .find_map(launch_id_from_cgroup_line)
        .context("current process is not in a Manifold launch cgroup")
}

fn launch_id_from_cgroup_line(line: &str) -> Option<String> {
    let path = line.rsplit_once(':').map_or(line, |(_, path)| path);
    let start = path.find(SCOPE_PREFIX)? + SCOPE_PREFIX.len();
    let rest = &path[start..];
    let end = rest.find(".scope")?;
    Some(rest[..end].to_string())
}

fn record_path(id: &str) -> Result<PathBuf> {
    Ok(registry_dir()?.join(format!("{id}.json")))
}

fn registry_dir() -> Result<PathBuf> {
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(runtime_dir).join("manifold").join("launches"));
    }
    bail!("XDG_RUNTIME_DIR is required for Manifold launch records")
}

pub fn os_args(args: &[OsString]) -> impl Iterator<Item = &OsStr> {
    args.iter().map(OsString::as_os_str)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::let_underscore_must_use,
    reason = "unit tests use direct temp directory setup and best-effort cleanup"
)]
mod tests {
    use super::{LaunchRecord, launch_id_from_cgroup_line};

    #[test]
    fn extracts_launch_id_from_systemd_scope_cgroup() {
        let line = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/manifold-launch-p123t456.scope";
        assert_eq!(
            launch_id_from_cgroup_line(line),
            Some("p123t456".to_string())
        );
    }

    #[test]
    fn launch_record_without_runner_cache_root_decodes() {
        let json = r#"{
            "id": "p123t456",
            "project_dir": "/tmp/project",
            "working_dir": "/tmp/project",
            "cache_dir": "/tmp/cache/sessions/p123",
            "runner_kind": "nix-develop",
            "program_argv": [],
            "session_args": null
        }"#;

        let record: LaunchRecord = serde_json::from_str(json).expect("decode launch record");

        assert_eq!(record.id, "p123t456");
        assert_eq!(record.runner_cache_root, None);
    }
}
