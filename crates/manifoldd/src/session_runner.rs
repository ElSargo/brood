use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::{
    RunArgs, config,
    config::SessionRunnerSelection,
    launch_context::{self, LaunchRecord},
};

const RUNNER_ACTIVE_ENV: &str = "MANIFOLD_SESSION_RUNNER_ACTIVE";

pub(crate) fn run_if_configured(args: &RunArgs) -> Result<bool> {
    if args.no_session_runner || env::var_os(RUNNER_ACTIVE_ENV).is_some() {
        return Ok(false);
    }

    let Some(selection) = runner_config(args)? else {
        return Ok(false);
    };

    let cache_dir = create_cache_dir(selection.runner.cache_root.as_deref())?;
    let current_exe = env::current_exe().context("resolve current exe")?;
    let mut session_args = args.session.clone();
    session_args.config = selection.config_path.clone();
    let record = LaunchRecord {
        id: launch_context::new_launch_id(),
        project_dir: selection.project_dir.clone(),
        working_dir: selection.project_dir.clone(),
        cache_dir: cache_dir.clone(),
        runner_kind: selection.runner.kind.clone(),
        program_argv: vec![session_server_path(&current_exe).into_os_string()],
        session_args: Some(session_args),
    };
    let mut command = runner_scope_command(&selection.runner, &current_exe, &record)?;

    info!(
        runner = %selection.runner.kind,
        runner_source = ?selection.source,
        runner_priority = selection.priority,
        launch = %record.id,
        project_dir = %selection.project_dir.display(),
        cache_dir = %cache_dir.display(),
        "starting Manifold session runner"
    );
    let status = command.status();
    launch_context::remove_launch_record(&record.id);
    cleanup_cache_dir(&cache_dir);
    let status = status.context("run Manifold session runner")?;
    if !status.success() {
        bail!("Manifold session runner exited with {status}");
    }
    Ok(true)
}

pub(crate) fn run_project_command_if_configured(
    selection: Option<&SessionRunnerSelection>,
    argv: &[OsString],
    extra_env: &[(String, OsString)],
    remove_env: &[&str],
) -> Result<Option<ExitStatus>> {
    let Some(selection) = selection else {
        return Ok(None);
    };
    if env::var_os(RUNNER_ACTIVE_ENV).is_some() {
        return Ok(None);
    }
    if argv.is_empty() {
        bail!("runner command argv must not be empty");
    }

    let working_dir = current_project_dir()?;
    let cache_dir = create_cache_dir(selection.runner.cache_root.as_deref())?;
    let current_exe = env::current_exe().context("resolve current exe")?;
    let record = LaunchRecord {
        id: launch_context::new_launch_id(),
        project_dir: selection.project_dir.clone(),
        working_dir,
        cache_dir: cache_dir.clone(),
        runner_kind: selection.runner.kind.clone(),
        program_argv: argv.to_vec(),
        session_args: None,
    };
    let mut command = runner_scope_command(&selection.runner, &current_exe, &record)?;
    command
        .env(RUNNER_ACTIVE_ENV, "1")
        .envs(extra_env.iter().map(|(key, value)| (key, value)));
    for key in remove_env {
        command.env_remove(key);
    }

    let status = command.status();
    launch_context::remove_launch_record(&record.id);
    cleanup_cache_dir(&cache_dir);
    status
        .map(Some)
        .context("run command through Manifold session runner")
}

fn runner_config(args: &RunArgs) -> Result<Option<SessionRunnerSelection>> {
    if let Some(kind) = args.session_runner.as_deref() {
        if kind == "none" || kind == "off" {
            return Ok(None);
        }
        return Ok(Some(config::override_session_runner(
            kind.to_string(),
            current_project_dir()?,
            args.session.config.clone(),
        )));
    }
    config::discover_session_runner(&current_project_dir()?)
}

fn runner_scope_command(
    runner: &config::SessionRunnerConfig,
    current_exe: &Path,
    record: &LaunchRecord,
) -> Result<Command> {
    let (program, args) = runner_program(runner, current_exe)?;
    launch_context::write_launch_record(record)?;

    let mut command = Command::new("systemd-run");
    command
        .arg("--user")
        .arg("--scope")
        .arg("--quiet")
        .arg("--collect")
        .arg("--unit")
        .arg(launch_context::scope_unit(&record.id))
        .arg("--working-directory")
        .arg(&record.working_dir)
        .arg(program)
        .args(args);
    Ok(command)
}

fn runner_program(
    runner: &config::SessionRunnerConfig,
    current_exe: &Path,
) -> Result<(OsString, Vec<OsString>)> {
    if let Some((program, args)) = runner.command.split_first() {
        return Ok((
            OsString::from(program.as_str()),
            args.iter()
                .map(|arg| OsString::from(arg.as_str()))
                .collect(),
        ));
    }

    match runner.kind.as_str() {
        "nix-develop" => Ok((
            sidecar_binary_path(current_exe, "manifold-runner-nix-develop").into_os_string(),
            Vec::new(),
        )),
        "script" | "custom" => bail!(
            "session runner kind={:?} requires command = [...]",
            runner.kind
        ),
        unsupported => bail!("unsupported session runner kind {unsupported:?}"),
    }
}

fn session_server_path(current_exe: &Path) -> PathBuf {
    sidecar_binary_path(current_exe, "manifold-sessiond")
}

fn sidecar_binary_path(current_exe: &Path, name: &str) -> PathBuf {
    let mut path = current_exe
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(name);
    if let Some(extension) = current_exe.extension() {
        path.set_extension(extension);
    }
    path
}

fn current_project_dir() -> Result<PathBuf> {
    let directory = env::current_dir().context("determine session project directory")?;
    Ok(directory.canonicalize().unwrap_or(directory))
}

fn create_cache_dir(cache_root: Option<&Path>) -> Result<PathBuf> {
    let root = cache_root
        .map(Path::to_path_buf)
        .unwrap_or_else(default_cache_root);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let cache_dir = root
        .join("sessions")
        .join(format!("{}-{stamp}", std::process::id()));
    fs::create_dir_all(&cache_dir).with_context(|| format!("create {}", cache_dir.display()))?;
    Ok(cache_dir)
}

fn default_cache_root() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(env::temp_dir)
        .join("manifold")
}

fn cleanup_cache_dir(cache_dir: &Path) {
    if let Err(err) = fs::remove_dir_all(cache_dir) {
        warn!(
            ?err,
            cache_dir = %cache_dir.display(),
            "failed to remove Manifold session runner cache directory"
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::launch_context;

    #[test]
    fn scope_unit_uses_launch_record_id() {
        assert_eq!(
            launch_context::scope_unit("p123t456"),
            "manifold-launch-p123t456.scope"
        );
    }
}
