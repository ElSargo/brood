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
    if let Ok(path) = record_path(id)
        && let Err(error) = fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(?error, path = %path.display(), "failed to remove Manifold launch record");
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
    path.rsplit('/').find_map(|component| {
        component
            .strip_prefix(SCOPE_PREFIX)?
            .strip_suffix(".scope")
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    })
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
    reason = "launch-context unit tests require decoded fixtures before checking exact values"
)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use super::{LaunchRecord, SCOPE_PREFIX, launch_id_from_cgroup_line};

    #[test]
    fn extracts_launch_id_from_systemd_scope_cgroup() {
        let line = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/manifold-launch-p123t456.scope";
        assert_eq!(
            launch_id_from_cgroup_line(line),
            Some("p123t456".to_string())
        );
    }

    #[test]
    fn launch_scope_parser_preserves_unicode_id_and_surrounding_components() {
        let id = "ascii-日本語-e\u{301}-👩🏽\u{200d}💻-🇳🇿";
        let line = format!("0::/ユーザー.slice/проект.service/{SCOPE_PREFIX}{id}.scope");

        assert_eq!(launch_id_from_cgroup_line(&line).as_deref(), Some(id));
    }

    #[test]
    fn launch_scope_parser_requires_an_exact_nonempty_path_component() {
        for malformed in [
            "0::/manifold-launch-.scope",
            "0::/manifold-launch-p123.scope.extra",
            "0::/prefix-manifold-launch-p123.scope",
            "0::/unrelated.scope",
        ] {
            assert_eq!(launch_id_from_cgroup_line(malformed), None, "{malformed}");
        }
    }

    #[test]
    fn launch_scope_parser_accepts_a_bare_scope_path() {
        assert_eq!(
            launch_id_from_cgroup_line("/app.slice/manifold-launch-p123t456.scope"),
            Some("p123t456".to_string())
        );
    }

    #[test]
    fn launch_scope_parser_does_not_truncate_a_long_multibyte_identifier() {
        let id = format!("p{}t{}", "日e\u{301}👩🏽\u{200d}💻🇳🇿".repeat(256), 456);
        let line = format!("0::/app.slice/{SCOPE_PREFIX}{id}.scope");

        assert_eq!(
            launch_id_from_cgroup_line(&line).as_deref(),
            Some(id.as_str())
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

    #[test]
    fn launch_record_round_trip_preserves_unicode_paths_and_argv() {
        let record = LaunchRecord {
            id: "p123t456".to_string(),
            project_dir: PathBuf::from("/tmp/プロジェクト-👩🏽\u{200d}💻"),
            working_dir: PathBuf::from("/tmp/e\u{301}tude-🇳🇿"),
            cache_dir: PathBuf::from("/tmp/кэш-🧑🏽\u{200d}🚀"),
            runner_cache_root: None,
            runner_kind: "proof".to_string(),
            program_argv: vec![OsString::from("参数-e\u{301}-👍🏽-🇯🇵")],
            session_args: None,
        };

        let encoded = serde_json::to_vec(&record).expect("encode launch record");
        let decoded: LaunchRecord = serde_json::from_slice(&encoded).expect("decode launch record");

        assert_eq!(decoded.project_dir, record.project_dir);
        assert_eq!(decoded.working_dir, record.working_dir);
        assert_eq!(decoded.cache_dir, record.cache_dir);
        assert_eq!(decoded.program_argv, record.program_argv);
    }
}
