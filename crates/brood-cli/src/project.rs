use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

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

#[must_use]
pub fn managed_worktree_root() -> PathBuf {
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
