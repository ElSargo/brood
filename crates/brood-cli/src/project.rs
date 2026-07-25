use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail, ensure};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectContext {
    pub id: String,
    pub name: String,
    pub source_directory: PathBuf,
    pub working_tree: PathBuf,
    pub branch: Option<String>,
    pub is_worktree: bool,
    pub is_git: bool,
}

#[must_use]
pub fn discover_project(directory: &Path) -> ProjectContext {
    let directory = canonical_or_original(directory);
    discover_git_project(&directory).unwrap_or_else(|| fallback_project(directory))
}

pub fn create_managed_worktree(project: &ProjectContext, branch: &str) -> Result<PathBuf> {
    ensure!(project.is_git, "project is not a Git repository");
    let branch = branch.trim();
    ensure!(!branch.is_empty(), "branch name cannot be empty");
    let checked = Command::new("git")
        .args(["check-ref-format", "--branch"])
        .arg(branch)
        .output()
        .context("validate Git branch name")?;
    ensure_command_succeeded(&checked, "validate Git branch name")?;

    let target = managed_worktree_directory(project, branch);
    if target.exists() {
        let existing = discover_git_project(&target);
        ensure!(
            existing.as_ref().is_some_and(|existing| {
                existing.id == project.id && existing.branch.as_deref() == Some(branch)
            }),
            "managed worktree path exists but is not branch {branch:?} of project {}: {}",
            project.name,
            target.display()
        );
        return Ok(target);
    }
    let parent = target
        .parent()
        .context("managed worktree path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create managed worktree directory {}", parent.display()))?;

    let branch_exists = git_command_succeeds(
        &project.source_directory,
        &["show-ref", "--verify", "--quiet"],
        Some(&format!("refs/heads/{branch}")),
    );
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&project.source_directory)
        .args(["worktree", "add"]);
    if branch_exists {
        command.arg(&target).arg(branch);
    } else {
        command.arg("-b").arg(branch).arg(&target).arg("HEAD");
    }
    let output = command.output().with_context(|| {
        format!(
            "create managed worktree for branch {branch:?} at {}",
            target.display()
        )
    })?;
    ensure_command_succeeded(&output, "create managed Git worktree")?;
    Ok(target)
}

#[must_use]
pub fn managed_worktree_directory(project: &ProjectContext, branch: &str) -> PathBuf {
    managed_worktree_root()
        .join(format!(
            "{}-{:016x}",
            path_slug(&project.name),
            stable_hash(project.id.as_bytes())
        ))
        .join(format!(
            "{}-{:016x}",
            path_slug(branch),
            stable_hash(branch.as_bytes())
        ))
}

fn discover_git_project(directory: &Path) -> Option<ProjectContext> {
    let working_tree = git_path(directory, &["rev-parse", "--show-toplevel"])?;
    let common_git_directory = git_path(directory, &["rev-parse", "--git-common-dir"])?;
    let common_git_directory = if common_git_directory.is_absolute() {
        canonical_or_original(&common_git_directory)
    } else {
        canonical_or_original(&working_tree.join(common_git_directory))
    };
    let source_directory = common_git_directory
        .file_name()
        .filter(|name| *name == ".git")
        .and_then(|_| common_git_directory.parent())
        .map_or_else(|| working_tree.clone(), canonical_or_original);
    let name = source_directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_string();
    let id = common_git_directory.to_string_lossy().into_owned();
    let branch = git_text(directory, &["branch", "--show-current"]).filter(|name| !name.is_empty());
    Some(ProjectContext {
        id,
        name,
        is_worktree: working_tree != source_directory,
        source_directory,
        working_tree,
        branch,
        is_git: true,
    })
}

fn fallback_project(directory: PathBuf) -> ProjectContext {
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_string();
    ProjectContext {
        id: directory.to_string_lossy().into_owned(),
        name,
        source_directory: directory.clone(),
        working_tree: directory,
        branch: None,
        is_worktree: false,
        is_git: false,
    }
}

fn git_path(directory: &Path, args: &[&str]) -> Option<PathBuf> {
    git_text(directory, args).map(PathBuf::from)
}

fn git_text(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_command_succeeds(directory: &Path, args: &[&str], trailing: Option<&str>) -> bool {
    let mut command = Command::new("git");
    command.arg("-C").arg(directory).args(args);
    if let Some(trailing) = trailing {
        command.arg(trailing);
    }
    command.status().is_ok_and(|status| status.success())
}

fn ensure_command_succeeded(output: &Output, action: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    if message.is_empty() {
        bail!("{action} failed with {}", output.status);
    }
    bail!("{action} failed: {message}")
}

fn managed_worktree_root() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .unwrap_or_else(env::temp_dir)
        .join("brood")
        .join("worktrees")
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn path_slug(value: &str) -> String {
    let mut slug = String::with_capacity(value.len());
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            slug.push(character);
            separator = false;
        } else if !separator && !slug.is_empty() {
            slug.push('-');
            separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "project".to_string()
    } else {
        slug
    }
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
