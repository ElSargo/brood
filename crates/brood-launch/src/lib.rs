//! Shared executable roles. Subprocesses retain their independent lifetimes.
use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

pub const ROLES: &[(&str, &str)] = &[
    ("client", "brood-client"),
    ("session", "brood-sessiond"),
    ("remote", "brood-remote"),
    ("capsule", "brood-capsuled"),
    ("computer-use-mcp", "brood-computer-use-mcp"),
    ("runner-exec", "brood-runner-exec"),
    ("runner-nix-develop", "brood-runner-nix-develop"),
    ("bmux", "bmux"),
    ("profile", "brood-profile"),
];

/// Accept both explicit subcommands and installed compatibility aliases.
#[must_use]
pub fn role() -> Option<&'static str> {
    let mut args = env::args_os();
    let executable = args.next()?;
    let name = Path::new(&executable).file_name()?.to_str()?;
    if let Some((role, _)) = ROLES.iter().find(|(_, alias)| *alias == name) {
        return Some(role);
    }
    let first = args.next()?;
    ROLES
        .iter()
        .find(|(role, _)| first == *role)
        .map(|(role, _)| *role)
}

/// Arguments for a component parser, retaining argv[0] for Clap.
#[must_use]
pub fn args() -> Vec<OsString> {
    let mut args: Vec<_> = env::args_os().collect();
    let alias = args
        .first()
        .and_then(|arg| Path::new(arg).file_name())
        .is_some_and(|name| ROLES.iter().any(|(_, alias)| name == *alias));
    if !alias && role().is_some() {
        args.remove(1);
    }
    args
}

/// Recognize a unified executable, including Nix's wrapper backing file.
#[must_use]
pub fn is_unified(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "brood" || name == ".brood-wrapped")
}

/// Build a command for either a unified executable or a legacy override.
#[must_use]
pub fn command(executable: &Path, role: &str) -> Command {
    let mut command = Command::new(executable);
    if is_unified(executable) {
        command.arg(role);
    }
    command
}

pub fn executable(variable: &str, alias: &str) -> std::io::Result<PathBuf> {
    if let Some(path) = env::var_os(variable) {
        return Ok(path.into());
    }
    let current = env::current_exe()?;
    if is_unified(&current) {
        return Ok(current);
    }
    let unified = current.with_file_name("brood");
    if unified.is_file() {
        return Ok(unified);
    }
    let sibling = current.with_file_name(alias);
    Ok(if sibling.is_file() {
        sibling
    } else {
        alias.into()
    })
}

#[must_use]
pub fn help() -> String {
    let mut text = String::from("Component modes (use MODE --help):\n");
    for (role, _) in ROLES {
        text.push_str("  ");
        text.push_str(role);
        text.push('\n');
    }
    text.push_str("  capabilities  Report this build's capabilities as JSON");
    text
}
