use std::{
    env,
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
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

const RUNNER_ACTIVE_ENV: &str = "BROOD_SESSION_RUNNER_ACTIVE";

pub fn run_if_configured(args: &RunArgs) -> Result<bool> {
    if args.no_session_runner || env::var_os(RUNNER_ACTIVE_ENV).is_some() {
        return Ok(false);
    }

    let working_dir = current_project_dir()?;
    let Some(selection) = runner_config(args)? else {
        return Ok(false);
    };

    let cache_root = runner_cache_root(selection.runner.cache_root.as_deref());
    let cache_dir = create_cache_dir(&cache_root)?;
    let current_exe = env::current_exe().context("resolve current exe")?;
    let inner_binary = runner_entry(&current_exe, &cache_dir, "brood-sessiond", "session")?;
    let session_args = args.session.clone();
    let record = LaunchRecord {
        id: launch_context::new_launch_id(),
        project_dir: selection.project_dir.clone(),
        working_dir,
        cache_dir: cache_dir.clone(),
        runner_cache_root: Some(cache_root),
        runner_kind: selection.runner.kind.clone(),
        program_argv: vec![inner_binary.clone().into_os_string()],
        session_args: Some(session_args),
    };
    let mut command =
        runner_scope_command(&selection.runner, &current_exe, &record, &inner_binary)?;

    info!(
        runner = %selection.runner.kind,
        runner_source = ?selection.source,
        runner_priority = selection.priority,
        launch = %record.id,
        project_dir = %selection.project_dir.display(),
        working_dir = %record.working_dir.display(),
        cache_dir = %cache_dir.display(),
        "starting Brood session runner"
    );
    let status = command.status();
    launch_context::remove_launch_record(&record.id);
    cleanup_cache_dir(&cache_dir);
    let status = status.context("run Brood session runner")?;
    if !status.success() {
        bail!("Brood session runner exited with {status}");
    }
    Ok(true)
}

pub fn run_project_command_if_configured(
    selection: Option<&SessionRunnerSelection>,
    working_dir: &Path,
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

    let cache_root = runner_cache_root(selection.runner.cache_root.as_deref());
    let cache_dir = create_cache_dir(&cache_root)?;
    let current_exe = env::current_exe().context("resolve current exe")?;
    let inner_binary = runner_entry(&current_exe, &cache_dir, "brood-runner-exec", "runner-exec")?;
    let record = LaunchRecord {
        id: launch_context::new_launch_id(),
        project_dir: selection.project_dir.clone(),
        working_dir: working_dir.to_path_buf(),
        cache_dir: cache_dir.clone(),
        runner_cache_root: Some(cache_root),
        runner_kind: selection.runner.kind.clone(),
        program_argv: argv.to_vec(),
        session_args: None,
    };
    let mut command =
        runner_scope_command(&selection.runner, &current_exe, &record, &inner_binary)?;
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
        .context("run command through Brood session runner")
}

fn runner_config(args: &RunArgs) -> Result<Option<SessionRunnerSelection>> {
    if let Some(kind) = args.session_runner.as_deref() {
        if kind == "none" || kind == "off" {
            return Ok(None);
        }
        return Ok(Some(config::override_session_runner(
            kind.to_string(),
            current_project_dir()?,
            args.session
                .config
                .clone()
                .unwrap_or_else(config::global_config_path),
        )));
    }
    config::discover_session_runner(&current_project_dir()?)
}

fn runner_scope_command(
    runner: &config::SessionRunnerConfig,
    current_exe: &Path,
    record: &LaunchRecord,
    inner_binary: &Path,
) -> Result<Command> {
    let (program, configured_args) = runner_program(runner, current_exe, &record.project_dir)?;
    let mut args = runner_activation_args(record, inner_binary);
    if runner.command.is_empty() && brood_launch::is_unified(current_exe) {
        args.insert(0, "runner-nix-develop".into());
    }
    args.extend(configured_args);
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

fn runner_activation_args(record: &LaunchRecord, inner_binary: &Path) -> Vec<OsString> {
    let runner_cache_root = record
        .runner_cache_root
        .as_deref()
        .unwrap_or(&record.cache_dir);

    vec![
        record.project_dir.clone().into_os_string(),
        record.working_dir.clone().into_os_string(),
        record.cache_dir.clone().into_os_string(),
        runner_cache_root.as_os_str().to_owned(),
        inner_binary.as_os_str().to_owned(),
    ]
}

fn runner_program(
    runner: &config::SessionRunnerConfig,
    current_exe: &Path,
    project_dir: &Path,
) -> Result<(OsString, Vec<OsString>)> {
    if let Some((program, args)) = runner.command.split_first() {
        return Ok((
            runner_command_program(program, project_dir),
            args.iter()
                .map(|arg| OsString::from(arg.as_str()))
                .collect(),
        ));
    }

    match runner.kind.as_str() {
        "nix-develop" => Ok((
            sidecar_binary_path(current_exe, "brood-runner-nix-develop").into_os_string(),
            Vec::new(),
        )),
        "script" | "custom" => bail!(
            "session runner kind={:?} requires command = [...]",
            runner.kind
        ),
        unsupported => bail!("unsupported session runner kind {unsupported:?}"),
    }
}

fn runner_command_program(program: &str, project_dir: &Path) -> OsString {
    if Path::new(program).is_absolute() || !program.contains('/') {
        OsString::from(program)
    } else {
        project_dir.join(program).into_os_string()
    }
}

// Preserve the custom runner's single executable argument. Its private entry
// selects the unified role and is removed with the launch's existing cache.
fn runner_entry(current_exe: &Path, cache_dir: &Path, alias: &str, role: &str) -> Result<PathBuf> {
    if !brood_launch::is_unified(current_exe) {
        return Ok(sidecar_binary_path(current_exe, alias));
    }
    let executable = current_exe
        .to_str()
        .context("runner executable path is not UTF-8")?;
    let quoted = executable.replace('\'', "'\\''");
    let entry = cache_dir.join(alias);
    fs::write(
        &entry,
        format!("#!/bin/sh\nexec '{quoted}' {role} \"$@\"\n"),
    )?;
    fs::set_permissions(&entry, fs::Permissions::from_mode(0o700))?;
    Ok(entry)
}

fn sidecar_binary_path(current_exe: &Path, name: &str) -> PathBuf {
    if brood_launch::is_unified(current_exe) {
        return current_exe.to_path_buf();
    }
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

fn create_cache_dir(cache_root: &Path) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let cache_dir = cache_root
        .join("sessions")
        .join(format!("{}-{stamp}", std::process::id()));
    fs::create_dir_all(&cache_dir).with_context(|| format!("create {}", cache_dir.display()))?;
    Ok(cache_dir)
}

fn runner_cache_root(configured_cache_root: Option<&Path>) -> PathBuf {
    configured_cache_root.map_or_else(default_cache_root, Path::to_path_buf)
}

fn default_cache_root() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(env::temp_dir)
        .join("brood")
}

fn cleanup_cache_dir(cache_dir: &Path) {
    if let Err(err) = fs::remove_dir_all(cache_dir) {
        warn!(
            ?err,
            cache_dir = %cache_dir.display(),
            "failed to remove Brood session runner cache directory"
        );
    }
}
