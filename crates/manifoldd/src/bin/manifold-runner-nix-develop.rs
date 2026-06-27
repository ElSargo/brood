use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use manifoldd::launch_context;
use std::os::unix::process::CommandExt;

fn main() -> Result<()> {
    let record = launch_context::current_launch_record()?;
    let project_ref = path_flake_ref(record.project_dir.clone());
    let exec_helper =
        sidecar_binary_path("manifold-runner-exec").context("resolve runner exec helper")?;

    let mut command = Command::new("nix");
    command
        .arg("develop")
        .arg("--no-write-lock-file")
        .arg(project_ref)
        .arg("--command")
        .arg(exec_helper)
        .current_dir(&record.working_dir)
        .envs(record.env_pairs());

    Err(command.exec()).context("exec nix develop runner")
}

fn path_flake_ref(path: PathBuf) -> OsString {
    OsString::from(format!("path:{}", path.display()))
}

fn sidecar_binary_path(name: &str) -> Result<PathBuf> {
    let current_exe = env::current_exe().context("resolve current executable")?;
    let mut path = current_exe
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(name);
    if let Some(extension) = current_exe.extension() {
        path.set_extension(extension);
    }
    Ok(path)
}
