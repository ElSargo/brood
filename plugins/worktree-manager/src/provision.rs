use super::{GitOutput, WorktreeRecord, command_error, ensure_success};
use std::{collections::BTreeSet, path::Path};

const CHECK_IGNORE_CHUNK_BYTES: usize = 16 * 1024;
const COMMAND_ARGUMENT_CHUNK_BYTES: usize = 16 * 1024;

pub(super) trait ProvisionRunner {
    async fn run(
        &self,
        command: &str,
        arguments: &[String],
        input: Option<&[u8]>,
    ) -> Result<GitOutput, String>;
}

pub(super) struct HostProvisionRunner;

impl ProvisionRunner for HostProvisionRunner {
    async fn run(
        &self,
        command: &str,
        arguments: &[String],
        input: Option<&[u8]>,
    ) -> Result<GitOutput, String> {
        super::run_command_with_input(command, arguments, input).await
    }
}

#[derive(Debug)]
struct TreeEntry {
    mode: String,
    object_type: String,
    path: String,
}

pub(super) async fn provision<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<(), String> {
    validate_context(runner, record).await?;
    if existing_worktree_matches(runner, record).await? {
        if provisional_marker_exists(runner, record).await? {
            rollback_registered_worktree(runner, record).await?;
        } else {
            return Ok(());
        }
    }
    if path_exists(runner, &record.path).await? {
        return Err("managed worktree target exists but is not the requested worktree".to_string());
    }

    let branch_ref = format!("refs/heads/{}", record.branch);
    let branch_head = git_optional_text(
        runner,
        &record.source_directory,
        &["rev-parse", "--verify", &branch_ref],
    )
    .await?;
    let source_head = git_text(
        runner,
        &record.source_worktree,
        &["rev-parse", "--verify", "HEAD"],
        "resolve requesting worktree HEAD",
    )
    .await?;
    let target_head = branch_head.clone().unwrap_or_else(|| source_head.clone());
    let same_head = target_head == source_head;

    let provisional = provisional_arguments(record, branch_head.is_some(), &source_head);
    let added = runner.run("git", &provisional, None).await?;
    if added.status == 0 {
        match prime_registered_worktree(runner, record, &target_head, same_head).await {
            Ok(()) => return Ok(()),
            Err(optimization_error) => {
                if let Err(rollback_error) = rollback_registered_worktree(runner, record).await {
                    return Err(format!(
                        "reflink priming failed: {optimization_error}; rollback failed: {rollback_error}"
                    ));
                }
                return ordinary_fallback(runner, record, &optimization_error).await;
            }
        }
    }

    let optimization_error = command_error(&added, "register provisional Git worktree");
    if existing_worktree_matches(runner, record).await? {
        return Ok(());
    }
    ordinary_fallback(runner, record, &optimization_error).await
}

async fn validate_context<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<(), String> {
    let branch = runner
        .run(
            "git",
            &[
                "check-ref-format".to_string(),
                "--branch".to_string(),
                record.branch.clone(),
            ],
            None,
        )
        .await?;
    ensure_success(&branch, "validate Git branch name")?;
    if record.source_worktree.is_empty() {
        return Err("managed worktree request has no requesting worktree".to_string());
    }
    if record.path != record.source_worktree
        && Path::new(&record.path).starts_with(&record.source_worktree)
    {
        return Err("managed worktree target must be outside its requesting worktree".to_string());
    }
    let top = git_text(
        runner,
        &record.source_worktree,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        "resolve requesting worktree",
    )
    .await?;
    if top != record.source_worktree {
        return Err(format!(
            "requesting worktree changed from {} to {top}",
            record.source_worktree
        ));
    }
    let common = git_text(
        runner,
        &record.source_worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        "resolve requesting worktree repository",
    )
    .await?;
    if common != record.project_id {
        return Err("requesting worktree no longer belongs to the requested project".to_string());
    }
    Ok(())
}

fn provisional_arguments(
    record: &WorktreeRecord,
    existing_branch: bool,
    source_head: &str,
) -> Vec<String> {
    let mut arguments = vec![
        "-C".to_string(),
        record.source_directory.clone(),
        "worktree".to_string(),
        "add".to_string(),
        "--no-checkout".to_string(),
    ];
    if existing_branch {
        arguments.extend([record.path.clone(), record.branch.clone()]);
    } else {
        arguments.extend([
            "-b".to_string(),
            record.branch.clone(),
            record.path.clone(),
            source_head.to_string(),
        ]);
    }
    arguments
}

async fn prime_registered_worktree<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
    target_head: &str,
    same_head: bool,
) -> Result<(), String> {
    create_provisional_marker(runner, record).await?;
    let tree = run_git(
        runner,
        &record.source_directory,
        &["ls-tree", "-rz", "--full-tree", target_head],
    )
    .await?;
    ensure_success(&tree, "enumerate target Git tree")?;
    let entries = parse_tree(&tree.stdout)?;
    if same_head {
        install_bulk_reflink_seed(runner, record).await?;
    } else {
        for entry in &entries {
            seed_tracked_path(runner, record, entry).await?;
        }
    }

    let reset = run_git(runner, &record.path, &["reset", "--mixed", target_head]).await?;
    ensure_success(&reset, "populate provisional worktree index")?;
    normalize_tracked_paths(runner, record).await?;

    if same_head {
        prune_bulk_seed(runner, record, &entries).await?;
    }
    verify_clean(runner, record).await?;
    remove_provisional_marker(runner, record).await
}

async fn install_bulk_reflink_seed<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<(), String> {
    let listed = runner
        .run(
            "find",
            &[
                record.source_worktree.clone(),
                "-mindepth".to_string(),
                "1".to_string(),
                "-maxdepth".to_string(),
                "1".to_string(),
                "!".to_string(),
                "-name".to_string(),
                ".git".to_string(),
                "-print0".to_string(),
            ],
            None,
        )
        .await?;
    ensure_success(&listed, "enumerate requesting worktree roots")?;
    let paths = nul_strings(&listed.stdout)?;
    for chunk in argument_chunks(&paths) {
        let mut arguments = vec![
            "--archive".to_string(),
            "--no-preserve=ownership".to_string(),
            "--reflink=always".to_string(),
            "--target-directory".to_string(),
            record.path.clone(),
            "--".to_string(),
        ];
        arguments.extend(chunk.iter().cloned());
        let copied = runner.run("cp", &arguments, None).await?;
        ensure_success(&copied, "reflink requesting worktree")?;
    }
    Ok(())
}

async fn seed_tracked_path<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
    entry: &TreeEntry,
) -> Result<(), String> {
    if entry.object_type != "blob" {
        return Ok(());
    }
    let source = join(&record.source_worktree, &entry.path)?;
    let target = join(&record.path, &entry.path)?;
    // This path is only used when an existing branch names a different tree.
    // Missing or differently typed files are populated later by Git.
    let Some(kind) = file_kind(runner, &source).await? else {
        return Ok(());
    };
    if entry.mode == "120000" && kind == "symbolic link" {
        ensure_parent(runner, &target).await?;
        copy_symlink(runner, &source, &target).await?;
    } else if entry.mode != "120000" && kind == "regular file" {
        ensure_parent(runner, &target).await?;
        clone_regular(runner, &source, &target).await?;
    }
    Ok(())
}

async fn normalize_tracked_paths<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<(), String> {
    let changed = run_git(runner, &record.path, &["diff-files", "--name-only", "-z"]).await?;
    ensure_success(&changed, "find tracked paths requiring checkout")?;
    if changed.stdout.is_empty() {
        return Ok(());
    }
    let checked_out = runner
        .run(
            "git",
            &[
                "-C".to_string(),
                record.path.clone(),
                "checkout-index".to_string(),
                "--force".to_string(),
                "--stdin".to_string(),
                "-z".to_string(),
            ],
            Some(&changed.stdout),
        )
        .await?;
    ensure_success(&checked_out, "normalize tracked worktree paths")
}

async fn prune_bulk_seed<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
    target_entries: &[TreeEntry],
) -> Result<(), String> {
    let (candidates, ignored_paths) =
        committed_ignored_inventory(runner, record, target_entries).await?;
    let protected_paths = target_entries
        .iter()
        .filter(|entry| entry.object_type == "blob")
        .map(|entry| entry.path.as_str())
        .chain(ignored_paths.iter().map(String::as_str))
        .collect::<Vec<_>>();
    let mut protected_directories = BTreeSet::new();
    for path in protected_paths {
        let mut ancestor = path;
        while let Some(separator) = ancestor.rfind('/') {
            ancestor = &ancestor[..separator];
            protected_directories.insert(ancestor.to_string());
        }
    }
    let mut removals = target_entries
        .iter()
        .filter(|entry| entry.object_type == "commit")
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();

    for (path, kind) in &candidates {
        let retained_ignored = ignored_paths.contains(path) && matches!(kind, 'd' | 'f' | 'l');
        let contains_protected_path = *kind == 'd' && protected_directories.contains(path);
        if !retained_ignored && !contains_protected_path {
            removals.insert(path.clone());
        }
    }

    let paths = removals
        .iter()
        .map(|relative| join(&record.path, relative))
        .collect::<Result<Vec<_>, _>>()?;
    for chunk in argument_chunks(&paths) {
        let mut arguments = vec!["-rf".to_string(), "--".to_string()];
        arguments.extend(chunk.iter().cloned());
        let removed = runner.run("rm", &arguments, None).await?;
        ensure_success(&removed, "prune unseeded worktree paths")?;
    }
    Ok(())
}

async fn committed_ignored_inventory<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
    target_entries: &[TreeEntry],
) -> Result<(Vec<(String, char)>, BTreeSet<String>), String> {
    let listed = runner
        .run(
            "find",
            &[
                record.source_worktree.clone(),
                "-xdev".to_string(),
                "-mindepth".to_string(),
                "1".to_string(),
                "(".to_string(),
                "-path".to_string(),
                join(&record.source_worktree, ".git")?,
                "-o".to_string(),
                "-path".to_string(),
                record.path.clone(),
                ")".to_string(),
                "-prune".to_string(),
                "-o".to_string(),
                "-printf".to_string(),
                "%y %p\\0".to_string(),
            ],
            None,
        )
        .await?;
    ensure_success(&listed, "enumerate requesting worktree paths")?;

    let tracked = run_git(runner, &record.source_worktree, &["ls-files", "-z"]).await?;
    ensure_success(&tracked, "enumerate requesting worktree tracked paths")?;
    let tracked = nul_strings(&tracked.stdout)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let submodules = target_entries
        .iter()
        .filter(|entry| entry.object_type == "commit")
        .map(|entry| entry.path.as_str())
        .collect::<Vec<_>>();

    let prefix = format!("{}/", record.source_worktree.trim_end_matches('/'));
    let candidates = nul_typed_paths(&listed.stdout)?
        .into_iter()
        .filter_map(|(kind, path)| {
            path.strip_prefix(&prefix)
                .map(|relative| (relative.to_string(), kind))
        })
        .filter(|(path, _kind)| !tracked.contains(path))
        .filter(|(path, _kind)| !path.split('/').any(|component| component == ".git"))
        .filter(|(path, _kind)| !submodules.iter().any(|root| is_within(path, root)))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok((candidates, BTreeSet::new()));
    }
    let classifier = classifier_git_directory(record, target_entries)?;
    let initialized = runner
        .run(
            "git",
            &["init".to_string(), "--bare".to_string(), classifier.clone()],
            None,
        )
        .await?;
    ensure_success(&initialized, "create committed-ignore classifier")?;
    // Keep each request below the bounded worker stream capacity. check-ignore
    // echoes matching paths, so an unbounded all-ignored input can otherwise
    // fill stdout while the host is still writing stdin.
    let classification = async {
        let mut ignored_paths = BTreeSet::new();
        let mut input = Vec::new();
        for (candidate, _kind) in &candidates {
            let required = candidate.len().saturating_add(1);
            if !input.is_empty() && input.len().saturating_add(required) > CHECK_IGNORE_CHUNK_BYTES
            {
                ignored_paths.extend(classify_ignored(runner, record, &classifier, &input).await?);
                input.clear();
            }
            input.extend_from_slice(candidate.as_bytes());
            input.push(0);
        }
        if !input.is_empty() {
            ignored_paths.extend(classify_ignored(runner, record, &classifier, &input).await?);
        }
        Ok::<_, String>(ignored_paths)
    }
    .await;
    let removed = runner
        .run(
            "rm",
            &["-rf".to_string(), "--".to_string(), classifier],
            None,
        )
        .await?;
    let cleanup = ensure_success(&removed, "remove committed-ignore classifier");
    match (classification, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(ignored_paths), Ok(())) => Ok((candidates, ignored_paths)),
    }
}

async fn classify_ignored<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
    classifier: &str,
    input: &[u8],
) -> Result<Vec<String>, String> {
    let ignored = runner
        .run(
            "git",
            &[
                "-c".to_string(),
                "core.excludesFile=/dev/null".to_string(),
                format!("--git-dir={classifier}"),
                "--work-tree".to_string(),
                record.path.clone(),
                "check-ignore".to_string(),
                "--no-index".to_string(),
                "--stdin".to_string(),
                "-z".to_string(),
            ],
            Some(input),
        )
        .await?;
    if !matches!(ignored.status, 0 | 1) {
        return Err(command_error(
            &ignored,
            "classify paths with target ignore rules",
        ));
    }
    nul_strings(&ignored.stdout)
}

fn classifier_git_directory(
    record: &WorktreeRecord,
    target_entries: &[TreeEntry],
) -> Result<String, String> {
    for suffix in 0_u8..=u8::MAX {
        let relative = format!(".brood-ignore-classifier-{suffix}.git");
        if !target_entries
            .iter()
            .any(|entry| is_within(&entry.path, &relative) || is_within(&relative, &entry.path))
        {
            return join(&record.path, &relative);
        }
    }
    Err("target tree occupies every ignore-classifier path".to_string())
}

async fn verify_clean<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<(), String> {
    let status = run_git(
        runner,
        &record.path,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .await?;
    ensure_success(&status, "verify primed Git worktree")?;
    if status.stdout.is_empty() {
        Ok(())
    } else {
        Err("primed Git worktree is not clean".to_string())
    }
}

async fn rollback_registered_worktree<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<(), String> {
    if !existing_worktree_matches(runner, record).await? {
        return Err("provisional target is no longer the registered managed worktree".to_string());
    }
    let removed = run_git(
        runner,
        &record.source_directory,
        &["worktree", "remove", "--force", &record.path],
    )
    .await?;
    ensure_success(&removed, "remove provisional Git worktree")
}

async fn provisional_marker<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<String, String> {
    git_text(
        runner,
        &record.path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "brood-reflink-provisioning",
        ],
        "resolve provisional worktree marker",
    )
    .await
}

async fn provisional_marker_exists<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<bool, String> {
    let marker = provisional_marker(runner, record).await?;
    path_exists(runner, &marker).await
}

async fn create_provisional_marker<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<(), String> {
    let marker = provisional_marker(runner, record).await?;
    ensure_directory(runner, &marker).await
}

async fn remove_provisional_marker<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<(), String> {
    let marker = provisional_marker(runner, record).await?;
    let removed = runner
        .run("rm", &["-rf".to_string(), "--".to_string(), marker], None)
        .await?;
    ensure_success(&removed, "remove provisional worktree marker")
}

async fn ordinary_fallback<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
    optimization_error: &str,
) -> Result<(), String> {
    let branch_ref = format!("refs/heads/{}", record.branch);
    let branch_exists = git_optional_text(
        runner,
        &record.source_directory,
        &["rev-parse", "--verify", &branch_ref],
    )
    .await?
    .is_some();
    let source_head = git_text(
        runner,
        &record.source_worktree,
        &["rev-parse", "--verify", "HEAD"],
        "resolve requesting worktree HEAD for fallback",
    )
    .await?;
    let mut arguments = vec![
        "-C".to_string(),
        record.source_directory.clone(),
        "worktree".to_string(),
        "add".to_string(),
    ];
    if branch_exists {
        arguments.extend([record.path.clone(), record.branch.clone()]);
    } else {
        arguments.extend([
            "-b".to_string(),
            record.branch.clone(),
            record.path.clone(),
            source_head,
        ]);
    }
    let fallback = runner.run("git", &arguments, None).await?;
    if fallback.status == 0 || existing_worktree_matches(runner, record).await? {
        return Ok(());
    }
    Err(format!(
        "reflink priming failed: {optimization_error}; ordinary Git fallback failed: {}",
        command_error(&fallback, "create managed Git worktree")
    ))
}

async fn existing_worktree_matches<R: ProvisionRunner>(
    runner: &R,
    record: &WorktreeRecord,
) -> Result<bool, String> {
    if !path_exists(runner, &record.path).await? {
        return Ok(false);
    }
    let branch = git_optional_text(runner, &record.path, &["branch", "--show-current"]).await?;
    let common = git_optional_text(
        runner,
        &record.path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    Ok(branch.as_deref() == Some(record.branch.as_str())
        && common.as_deref() == Some(record.project_id.as_str()))
}

async fn path_exists<R: ProvisionRunner>(runner: &R, path: &str) -> Result<bool, String> {
    let output = runner
        .run("test", &["-e".to_string(), path.to_string()], None)
        .await?;
    match output.status {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(command_error(&output, "inspect managed worktree target")),
    }
}

async fn file_kind<R: ProvisionRunner>(runner: &R, path: &str) -> Result<Option<String>, String> {
    let output = runner
        .run(
            "stat",
            &[
                "--format=%F".to_string(),
                "--".to_string(),
                path.to_string(),
            ],
            None,
        )
        .await?;
    match output.status {
        0 => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        1 => Ok(None),
        _ => Err(command_error(&output, "inspect reflink source path")),
    }
}

async fn ensure_parent<R: ProvisionRunner>(runner: &R, path: &str) -> Result<(), String> {
    let parent = Path::new(path)
        .parent()
        .ok_or_else(|| format!("path has no parent: {path}"))?
        .to_string_lossy()
        .into_owned();
    ensure_directory(runner, &parent).await
}

async fn ensure_directory<R: ProvisionRunner>(runner: &R, path: &str) -> Result<(), String> {
    let output = runner
        .run(
            "mkdir",
            &["-p".to_string(), "--".to_string(), path.to_string()],
            None,
        )
        .await?;
    ensure_success(&output, "create reflink target directory")
}

async fn clone_regular<R: ProvisionRunner>(
    runner: &R,
    source: &str,
    target: &str,
) -> Result<(), String> {
    let output = runner
        .run(
            "cp",
            &[
                "--reflink=always".to_string(),
                "--preserve=mode,timestamps".to_string(),
                "--".to_string(),
                source.to_string(),
                target.to_string(),
            ],
            None,
        )
        .await?;
    ensure_success(&output, "reflink worktree seed")
}

async fn copy_symlink<R: ProvisionRunner>(
    runner: &R,
    source: &str,
    target: &str,
) -> Result<(), String> {
    let output = runner
        .run(
            "cp",
            &[
                "--no-dereference".to_string(),
                "--".to_string(),
                source.to_string(),
                target.to_string(),
            ],
            None,
        )
        .await?;
    ensure_success(&output, "preserve worktree seed symlink")
}

async fn run_git<R: ProvisionRunner>(
    runner: &R,
    directory: &str,
    arguments: &[&str],
) -> Result<GitOutput, String> {
    let mut complete = vec!["-C".to_string(), directory.to_string()];
    complete.extend(arguments.iter().map(|argument| (*argument).to_string()));
    runner.run("git", &complete, None).await
}

async fn git_text<R: ProvisionRunner>(
    runner: &R,
    directory: &str,
    arguments: &[&str],
    action: &str,
) -> Result<String, String> {
    let output = run_git(runner, directory, arguments).await?;
    ensure_success(&output, action)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn git_optional_text<R: ProvisionRunner>(
    runner: &R,
    directory: &str,
    arguments: &[&str],
) -> Result<Option<String>, String> {
    let output = run_git(runner, directory, arguments).await?;
    match output.status {
        0 => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        1 | 128 => Ok(None),
        _ => Err(command_error(&output, "query Git repository")),
    }
}

fn parse_tree(bytes: &[u8]) -> Result<Vec<TreeEntry>, String> {
    nul_slices(bytes)
        .map(|record| {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| "target tree entry has no path".to_string())?;
            let header = std::str::from_utf8(&record[..tab])
                .map_err(|_| "target tree entry header is not UTF-8".to_string())?;
            let mut fields = header.split(' ');
            let mode = fields.next().unwrap_or_default().to_string();
            let object_type = fields.next().unwrap_or_default().to_string();
            if mode.is_empty() || object_type.is_empty() {
                return Err("target tree entry header is incomplete".to_string());
            }
            let path = std::str::from_utf8(&record[tab + 1..])
                .map_err(|_| "target tree path is not UTF-8".to_string())?
                .to_string();
            Ok(TreeEntry {
                mode,
                object_type,
                path,
            })
        })
        .collect()
}

fn nul_strings(bytes: &[u8]) -> Result<Vec<String>, String> {
    nul_slices(bytes)
        .map(|value| {
            std::str::from_utf8(value)
                .map(str::to_string)
                .map_err(|_| "Git path is not UTF-8".to_string())
        })
        .collect()
}

fn nul_typed_paths(bytes: &[u8]) -> Result<Vec<(char, String)>, String> {
    nul_slices(bytes)
        .map(|value| {
            if value.len() < 3 || value[1] != b' ' {
                return Err("find path record has no type prefix".to_string());
            }
            let kind = char::from(value[0]);
            let path = std::str::from_utf8(&value[2..])
                .map_err(|_| "find path is not UTF-8".to_string())?
                .to_string();
            Ok((kind, path))
        })
        .collect()
}

fn nul_slices(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
}

fn argument_chunks(paths: &[String]) -> Vec<&[String]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0_usize;
    for (index, path) in paths.iter().enumerate() {
        let required = path.len().saturating_add(1);
        if index > start && bytes.saturating_add(required) > COMMAND_ARGUMENT_CHUNK_BYTES {
            chunks.push(&paths[start..index]);
            start = index;
            bytes = 0;
        }
        bytes = bytes.saturating_add(required);
    }
    if start < paths.len() {
        chunks.push(&paths[start..]);
    }
    chunks
}

fn join(root: &str, relative: &str) -> Result<String, String> {
    if relative.is_empty()
        || Path::new(relative).is_absolute()
        || Path::new(relative)
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!("unsafe Git worktree path: {relative:?}"));
    }
    Ok(Path::new(root)
        .join(relative)
        .to_string_lossy()
        .into_owned())
}

fn is_within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        io::Write,
        os::unix::fs::symlink,
        process::{Command, Stdio},
    };

    struct NativeRunner {
        fail_reflink: bool,
        reflink_calls: Cell<usize>,
        reflink_successes: Cell<usize>,
        check_ignore_input_bytes: Cell<usize>,
        max_check_ignore_input_bytes: Cell<usize>,
    }

    impl NativeRunner {
        fn real() -> Self {
            Self {
                fail_reflink: false,
                reflink_calls: Cell::new(0),
                reflink_successes: Cell::new(0),
                check_ignore_input_bytes: Cell::new(0),
                max_check_ignore_input_bytes: Cell::new(0),
            }
        }

        fn failing_reflink() -> Self {
            Self {
                fail_reflink: true,
                reflink_calls: Cell::new(0),
                reflink_successes: Cell::new(0),
                check_ignore_input_bytes: Cell::new(0),
                max_check_ignore_input_bytes: Cell::new(0),
            }
        }
    }

    impl ProvisionRunner for NativeRunner {
        async fn run(
            &self,
            command: &str,
            arguments: &[String],
            input: Option<&[u8]>,
        ) -> Result<GitOutput, String> {
            let reflink = command == "cp" && arguments.iter().any(|arg| arg == "--reflink=always");
            if command == "git" && arguments.iter().any(|argument| argument == "check-ignore") {
                let input_bytes = input.map_or(0, <[u8]>::len);
                self.check_ignore_input_bytes.set(
                    self.check_ignore_input_bytes
                        .get()
                        .saturating_add(input_bytes),
                );
                self.max_check_ignore_input_bytes
                    .set(self.max_check_ignore_input_bytes.get().max(input_bytes));
            }
            if reflink {
                self.reflink_calls
                    .set(self.reflink_calls.get().saturating_add(1));
                if self.fail_reflink {
                    return Ok(GitOutput {
                        status: 95,
                        stdout: Vec::new(),
                        stderr: b"injected reflink failure".to_vec(),
                    });
                }
            }
            let mut child = Command::new(command)
                .args(arguments)
                .stdin(if input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| format!("spawn {command}: {error}"))?;
            if let Some(input) = input {
                child
                    .stdin
                    .take()
                    .ok_or_else(|| format!("open {command} stdin"))?
                    .write_all(input)
                    .map_err(|error| format!("write {command} stdin: {error}"))?;
            }
            let output = child
                .wait_with_output()
                .map_err(|error| format!("wait for {command}: {error}"))?;
            let output = GitOutput {
                status: output.status.code().unwrap_or(1),
                stdout: output.stdout,
                stderr: output.stderr,
            };
            if reflink && output.status == 0 {
                self.reflink_successes
                    .set(self.reflink_successes.get().saturating_add(1));
            }
            Ok(output)
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
            std::thread::yield_now();
        }
    }

    struct Repository {
        _root: tempfile::TempDir,
        source: String,
        target: String,
        common: String,
    }

    impl Repository {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temporary repository root");
            let source = root.path().join("source worktree");
            let target = root.path().join("managed target");
            std::fs::create_dir(&source).expect("source directory");
            command_ok(&source, &["git", "init", "--initial-branch=main"]);
            command_ok(
                &source,
                &["git", "config", "user.email", "test@example.invalid"],
            );
            command_ok(&source, &["git", "config", "user.name", "Brood Test"]);
            let common = command_text(
                &source,
                &[
                    "git",
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-common-dir",
                ],
            );
            Self {
                _root: root,
                source: source.to_string_lossy().into_owned(),
                target: target.to_string_lossy().into_owned(),
                common,
            }
        }

        fn record(&self, branch: &str) -> WorktreeRecord {
            WorktreeRecord {
                project_id: self.common.clone(),
                project_name: "test".to_string(),
                source_directory: self.source.clone(),
                source_worktree: self.source.clone(),
                branch: branch.to_string(),
                path: self.target.clone(),
                metadata_path: format!("{}.json", self.target),
                session: None,
                last_used_unix: 0,
            }
        }

        fn path(&self, relative: &str) -> std::path::PathBuf {
            Path::new(&self.source).join(relative)
        }

        fn target_path(&self, relative: &str) -> std::path::PathBuf {
            Path::new(&self.target).join(relative)
        }

        fn commit(&self, message: &str) -> String {
            command_ok(Path::new(&self.source), &["git", "add", "-A"]);
            command_ok(Path::new(&self.source), &["git", "commit", "-m", message]);
            command_text(Path::new(&self.source), &["git", "rev-parse", "HEAD"])
        }
    }

    #[test]
    fn primes_target_tree_and_committed_ignored_data_from_requesting_worktree() {
        let repository = Repository::new();
        std::fs::write(repository.path(".gitignore"), "ignored dir/\n*.cache\n")
            .expect("write ignore rules");
        std::fs::write(
            repository.path("tracked space\nline.txt"),
            vec![b'a'; 1024 * 1024],
        )
        .expect("write tracked file");
        std::fs::create_dir(repository.path("mixed directory"))
            .expect("create tracked parent directory");
        std::fs::write(repository.path("mixed directory/tracked"), "tracked")
            .expect("write nested tracked file");
        symlink("tracked space\nline.txt", repository.path("tracked-link"))
            .expect("tracked symlink");
        let head = repository.commit("initial");

        std::fs::write(repository.path("tracked space\nline.txt"), "dirty source")
            .expect("modify tracked source");
        std::fs::write(repository.path(".gitignore"), "different.cache\n")
            .expect("modify source ignore file");
        std::fs::create_dir(repository.path("ignored dir")).expect("ignored directory");
        std::fs::write(
            repository.path("ignored dir/artifact.cache"),
            vec![b'z'; 512 * 1024],
        )
        .expect("ignored artifact");
        symlink("artifact.cache", repository.path("ignored dir/cache-link"))
            .expect("ignored symlink");
        command_ok(
            Path::new(&repository.source),
            &["mkfifo", "ignored dir/build.fifo"],
        );
        std::fs::write(repository.path("not ignored.txt"), "local only")
            .expect("untracked source file");
        std::fs::write(repository.path("mixed directory/local"), "local only")
            .expect("nested untracked source file");
        std::fs::write(
            Path::new(&repository.common).join("info/exclude"),
            "info-only/\n",
        )
        .expect("repository-local exclude rule");
        std::fs::create_dir(repository.path("info-only")).expect("info-only directory");
        std::fs::write(repository.path("info-only/cache"), "must not be seeded")
            .expect("info-only cache");

        let runner = NativeRunner::real();
        block_on(provision(&runner, &repository.record("feature/space-name")))
            .expect("provision reflink worktree");

        if runner.reflink_successes.get() == 0 {
            // The filesystem rejected FICLONE and provisioning correctly used
            // the ordinary Git fallback. The dedicated fallback test covers
            // the transaction; optimized-path assertions are inapplicable.
            assert_eq!(
                command_text(
                    Path::new(&repository.target),
                    &["git", "status", "--porcelain=v1", "--untracked-files=all"]
                ),
                ""
            );
            assert!(!repository.target_path("not ignored.txt").exists());
            return;
        }
        assert_eq!(runner.reflink_successes.get(), 1);
        assert_eq!(
            command_text(Path::new(&repository.target), &["git", "rev-parse", "HEAD"]),
            head
        );
        assert_eq!(
            std::fs::read(repository.target_path("tracked space\nline.txt"))
                .expect("read normalized target file"),
            vec![b'a'; 1024 * 1024]
        );
        assert!(
            repository
                .target_path("ignored dir/artifact.cache")
                .is_file()
        );
        assert_shared_extents_when_filefrag_is_available(
            &repository.path("ignored dir/artifact.cache"),
            &repository.target_path("ignored dir/artifact.cache"),
        );
        assert_eq!(
            std::fs::read_link(repository.target_path("ignored dir/cache-link"))
                .expect("read ignored link"),
            Path::new("artifact.cache")
        );
        assert!(!repository.target_path("ignored dir/build.fifo").exists());
        assert!(!repository.target_path("not ignored.txt").exists());
        assert!(repository.target_path("mixed directory/tracked").is_file());
        assert!(!repository.target_path("mixed directory/local").exists());
        assert!(!repository.target_path("info-only/cache").exists());
        assert_eq!(
            command_text(
                Path::new(&repository.target),
                &["git", "status", "--porcelain=v1", "--untracked-files=all"]
            ),
            ""
        );
    }

    #[test]
    fn existing_branch_keeps_its_head_and_does_not_receive_ignored_seed() {
        let repository = Repository::new();
        std::fs::write(repository.path(".gitignore"), "build/\n").expect("ignore rule");
        std::fs::write(repository.path("version"), "old").expect("old version");
        let old_head = repository.commit("old");
        command_ok(
            Path::new(&repository.source),
            &["git", "branch", "existing", &old_head],
        );
        std::fs::write(repository.path("version"), "new").expect("new version");
        repository.commit("new");
        std::fs::create_dir(repository.path("build")).expect("build directory");
        std::fs::write(repository.path("build/cache"), "cached").expect("build cache");

        block_on(provision(
            &NativeRunner::real(),
            &repository.record("existing"),
        ))
        .expect("provision existing branch");

        assert_eq!(
            command_text(Path::new(&repository.target), &["git", "rev-parse", "HEAD"]),
            old_head
        );
        assert_eq!(
            std::fs::read_to_string(repository.target_path("version"))
                .expect("read old target version"),
            "old"
        );
        assert!(!repository.target_path("build/cache").exists());
    }

    #[test]
    fn new_branch_uses_the_requesting_linked_worktree_and_head() {
        let repository = Repository::new();
        std::fs::write(repository.path("version"), "primary").expect("primary version");
        repository.commit("primary");
        let linked = repository._root.path().join("requesting linked worktree");
        command_ok(
            Path::new(&repository.source),
            &[
                "git",
                "worktree",
                "add",
                "-b",
                "session-source",
                linked.to_str().expect("linked path"),
            ],
        );
        std::fs::write(linked.join("version"), "session").expect("session version");
        command_ok(&linked, &["git", "add", "version"]);
        command_ok(&linked, &["git", "commit", "-m", "session head"]);
        let session_head = command_text(&linked, &["git", "rev-parse", "HEAD"]);
        let primary_head =
            command_text(Path::new(&repository.source), &["git", "rev-parse", "HEAD"]);
        assert_ne!(session_head, primary_head);

        let mut record = repository.record("from-session");
        record.source_worktree = linked.to_string_lossy().into_owned();
        block_on(provision(&NativeRunner::real(), &record))
            .expect("provision from linked requesting worktree");

        assert_eq!(
            command_text(Path::new(&repository.target), &["git", "rev-parse", "HEAD"]),
            session_head
        );
        assert_eq!(
            std::fs::read_to_string(repository.target_path("version"))
                .expect("read session target version"),
            "session"
        );
    }

    #[test]
    fn ignored_classification_chunks_more_than_one_stream_capacity() {
        let repository = Repository::new();
        std::fs::write(repository.path(".gitignore"), "ignored-*\n").expect("write ignore rule");
        repository.commit("ignore rule");
        std::fs::create_dir(&repository.target).expect("create classification target");
        std::fs::copy(
            repository.path(".gitignore"),
            repository.target_path(".gitignore"),
        )
        .expect("copy committed ignore rule");

        let mut candidate_bytes = 0_usize;
        let mut candidates = Vec::new();
        for index in 0..320 {
            let name = format!("ignored-{index:04}-{}", "x".repeat(220));
            candidate_bytes = candidate_bytes.saturating_add(name.len().saturating_add(1));
            std::fs::create_dir(repository.path(&name)).expect("create ignored directory");
            candidates.push(name);
        }
        assert!(candidate_bytes > 64 * 1024);

        let runner = NativeRunner::real();
        let (inventory, ignored) = block_on(committed_ignored_inventory(
            &runner,
            &repository.record("unused"),
            &[],
        ))
        .expect("classify ignored directories in bounded chunks");

        assert_eq!(runner.check_ignore_input_bytes.get(), candidate_bytes);
        assert!(runner.max_check_ignore_input_bytes.get() <= CHECK_IGNORE_CHUNK_BYTES);
        assert_eq!(inventory.len(), candidates.len());
        for candidate in candidates {
            assert!(ignored.contains(&candidate));
        }
    }

    #[test]
    fn reflink_failure_rolls_back_and_uses_ordinary_git_without_partial_seed() {
        let repository = Repository::new();
        std::fs::write(repository.path("tracked"), "tracked").expect("tracked file");
        repository.commit("initial");
        let runner = NativeRunner::failing_reflink();

        block_on(provision(&runner, &repository.record("fallback")))
            .expect("ordinary Git fallback");

        assert_eq!(runner.reflink_calls.get(), 1);
        assert_eq!(
            command_text(
                Path::new(&repository.target),
                &["git", "status", "--porcelain=v1"]
            ),
            ""
        );
        let worktrees = command_text(
            Path::new(&repository.source),
            &["git", "worktree", "list", "--porcelain"],
        );
        assert_eq!(worktrees.matches(&repository.target).count(), 1);
    }

    #[test]
    fn interrupted_provisional_registration_is_repaired_before_retry() {
        let repository = Repository::new();
        std::fs::write(repository.path("tracked"), "tracked").expect("tracked file");
        repository.commit("initial");
        command_ok(
            Path::new(&repository.source),
            &[
                "git",
                "worktree",
                "add",
                "--no-checkout",
                "-b",
                "interrupted",
                &repository.target,
                "HEAD",
            ],
        );
        let marker = command_text(
            Path::new(&repository.target),
            &[
                "git",
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "brood-reflink-provisioning",
            ],
        );
        std::fs::create_dir(&marker).expect("interrupted marker");

        block_on(provision(
            &NativeRunner::real(),
            &repository.record("interrupted"),
        ))
        .expect("repair interrupted worktree");

        assert!(repository.target_path("tracked").is_file());
        assert!(!Path::new(&marker).exists());
        assert_eq!(
            command_text(
                Path::new(&repository.target),
                &["git", "status", "--porcelain=v1"]
            ),
            ""
        );
    }

    fn command_ok(directory: &Path, arguments: &[&str]) {
        let (command, arguments) = arguments.split_first().expect("command");
        let output = Command::new(command)
            .current_dir(directory)
            .args(arguments)
            .output()
            .expect("run command");
        assert!(
            output.status.success(),
            "{} failed: {}",
            command,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn assert_shared_extents_when_filefrag_is_available(source: &Path, target: &Path) {
        let output = match Command::new("filefrag")
            .arg("-v")
            .arg(source)
            .arg(target)
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("run filefrag: {error}"),
        };
        assert!(
            output.status.success(),
            "filefrag failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let extents = String::from_utf8_lossy(&output.stdout);
        assert!(
            extents.contains("shared"),
            "reflinked source and target have no shared FIEMAP extent:\n{extents}"
        );
    }

    fn command_text(directory: &Path, arguments: &[&str]) -> String {
        let (command, arguments) = arguments.split_first().expect("command");
        let output = Command::new(command)
            .current_dir(directory)
            .args(arguments)
            .output()
            .expect("run command");
        assert!(
            output.status.success(),
            "{} failed: {}",
            command,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}
