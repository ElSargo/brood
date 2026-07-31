use brood_plugin_sdk::{
    actor::{send_presentation, spawn_worker},
    bindings,
    brood::plugin::host,
    exports::brood::plugin::actor::{
        Command, Event, Guest, LifecycleEvent, WorkerEvent, WorkerFinished,
    },
    runtime::{sleep as wait_timer, wait_child, wait_notification},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

const METADATA_BYTES: usize = 4096;
const MAINTENANCE_INTERVAL_MS: u64 = 60 * 60 * 1000;
const UNUSED_TTL_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum HostRequest {
    CreateWorktreeSession {
        request: u64,
        source_session: u64,
        branch: String,
    },
    ListDirectories {
        request: u64,
        #[serde(default)]
        roots: Vec<String>,
    },
    Acquire {
        request: u64,
        project_id: String,
        project_name: String,
        source_directory: String,
        branch: String,
        target: String,
        metadata_path: String,
        now: u64,
    },
    Attached {
        request: u64,
        session: u64,
        now: u64,
    },
    Released {
        session: u64,
        now: u64,
    },
    Bootstrap {
        records: Vec<WorktreeRecord>,
        #[serde(default)]
        home_directory: Option<String>,
    },
    Maintenance {
        now: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WorktreeRecord {
    project_id: String,
    project_name: String,
    source_directory: String,
    branch: String,
    path: String,
    metadata_path: String,
    session: Option<u64>,
    last_used_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum WorkerInput {
    Provision {
        request: u64,
        record: WorktreeRecord,
    },
    Persist {
        record: WorktreeRecord,
    },
    Cleanup {
        record: WorktreeRecord,
    },
    ListDirectories {
        request: u64,
        roots: Vec<String>,
    },
    Timer,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkerOutput {
    success: bool,
    message: String,
    request: Option<u64>,
    record: Option<WorktreeRecord>,
    #[serde(default)]
    directories: Vec<String>,
}

#[derive(Clone, Debug)]
enum Job {
    Provision { request: u64, path: String },
    Persist,
    Cleanup { path: String },
    ListDirectories { request: u64, actor: String },
    Timer,
}

struct ManagerState {
    next_worker: u64,
    jobs: BTreeMap<u64, Job>,
    records: BTreeMap<String, WorktreeRecord>,
    request_paths: BTreeMap<u64, String>,
    acquiring: BTreeSet<String>,
    home_directory: Option<String>,
    timer_pending: bool,
}

impl ManagerState {
    const fn new() -> Self {
        Self {
            next_worker: 1,
            jobs: BTreeMap::new(),
            records: BTreeMap::new(),
            request_paths: BTreeMap::new(),
            acquiring: BTreeSet::new(),
            home_directory: None,
            timer_pending: false,
        }
    }

    fn spawn(&mut self, job: Job, input: WorkerInput) -> Option<Command> {
        let worker = self.next_worker;
        self.next_worker = self.next_worker.saturating_add(1);
        let payload = serde_json::to_vec(&input).ok()?;
        self.jobs.insert(worker, job);
        Some(spawn_worker(worker, payload))
    }

    fn schedule_timer(&mut self) -> Option<Command> {
        if self.timer_pending {
            return None;
        }
        self.timer_pending = true;
        self.spawn(Job::Timer, WorkerInput::Timer)
    }

    fn persist(&mut self, record: WorktreeRecord) -> Option<Command> {
        self.spawn(Job::Persist, WorkerInput::Persist { record })
    }
}

static STATE: Mutex<ManagerState> = Mutex::new(ManagerState::new());

struct WorktreeManager;

impl Guest for WorktreeManager {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(_)) => {
                vec![Command::LifecycleComplete(Ok(()))]
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![Command::LifecycleComplete(Ok(()))]
            }
            Event::Mailbox(message) => handle_host_request(&message.sender, &message.body),
            Event::Worker(WorkerEvent::Finished(finished)) => handle_worker_finished(finished),
            Event::Worker(WorkerEvent::Failed(failed)) => {
                handle_worker_failed(failed.worker, &failed.message)
            }
            Event::Worker(WorkerEvent::Cancelled(worker)) => {
                handle_worker_failed(worker, "worker cancelled")
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        let input: WorkerInput =
            serde_json::from_slice(&payload).map_err(|error| format!("decode job: {error}"))?;
        let output = match input {
            WorkerInput::Provision { request, record } => match provision_worktree(&record).await {
                Ok(()) => WorkerOutput {
                    success: true,
                    message: String::new(),
                    request: Some(request),
                    record: Some(record),
                    directories: Vec::new(),
                },
                Err(message) => WorkerOutput {
                    success: false,
                    message,
                    request: Some(request),
                    record: None,
                    directories: Vec::new(),
                },
            },
            WorkerInput::Persist { record } => WorkerOutput {
                success: persist_record(&record).await.is_ok(),
                message: String::new(),
                request: None,
                record: Some(record),
                directories: Vec::new(),
            },
            WorkerInput::Cleanup { record } => match cleanup_worktree(&record).await {
                Ok(()) => WorkerOutput {
                    success: true,
                    message: String::new(),
                    request: None,
                    record: Some(record),
                    directories: Vec::new(),
                },
                Err(message) => WorkerOutput {
                    success: false,
                    message,
                    request: None,
                    record: Some(record),
                    directories: Vec::new(),
                },
            },
            WorkerInput::ListDirectories { request, roots } => {
                match list_directories(&roots).await {
                    Ok(directories) => WorkerOutput {
                        success: true,
                        message: String::new(),
                        request: Some(request),
                        record: None,
                        directories,
                    },
                    Err(message) => WorkerOutput {
                        success: false,
                        message,
                        request: Some(request),
                        record: None,
                        directories: Vec::new(),
                    },
                }
            }
            WorkerInput::Timer => {
                wait_timer(MAINTENANCE_INTERVAL_MS).await?;
                WorkerOutput {
                    success: true,
                    message: String::new(),
                    request: None,
                    record: None,
                    directories: Vec::new(),
                }
            }
        };
        serde_json::to_vec(&output).map_err(|error| format!("encode job result: {error}"))
    }
}

fn handle_host_request(sender: &str, bytes: &[u8]) -> Vec<Command> {
    let Ok(request) = serde_json::from_slice::<HostRequest>(bytes) else {
        return Vec::new();
    };
    let authorized = match (&request, sender) {
        (
            HostRequest::CreateWorktreeSession { .. } | HostRequest::ListDirectories { .. },
            sender,
        ) => sender.starts_with("presentation:"),
        (HostRequest::Acquire { .. }, "host:control")
        | (HostRequest::Attached { .. } | HostRequest::Released { .. }, "host:session")
        | (HostRequest::Bootstrap { .. }, "host")
        | (HostRequest::Maintenance { .. }, "host:maintenance") => true,
        _ => false,
    };
    if !authorized {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match request {
        HostRequest::CreateWorktreeSession {
            request,
            source_session,
            branch,
        } => {
            let value = serde_json::json!({
                "type": "worktree-session-requested",
                "request": request,
                "source_session": source_session,
                "branch": branch,
                "presentation_actor": sender
                    .strip_prefix("presentation:")
                    .unwrap_or(sender),
            });
            serde_json::to_vec(&value)
                .ok()
                .map(Command::SendHostMessage)
                .into_iter()
                .collect()
        }
        HostRequest::ListDirectories { request, mut roots } => {
            roots.push("/".to_string());
            if let Some(home) = state.home_directory.clone() {
                roots.push(home);
            }
            roots.retain(|root| root.starts_with('/'));
            roots.sort();
            roots.dedup();
            let actor = sender
                .strip_prefix("presentation:")
                .unwrap_or(sender)
                .to_string();
            state
                .spawn(
                    Job::ListDirectories { request, actor },
                    WorkerInput::ListDirectories { request, roots },
                )
                .into_iter()
                .collect()
        }
        HostRequest::Acquire {
            request,
            project_id,
            project_name,
            source_directory,
            branch,
            target,
            metadata_path,
            now,
        } => {
            let record = WorktreeRecord {
                project_id,
                project_name,
                source_directory,
                branch,
                path: target,
                metadata_path,
                session: None,
                last_used_unix: now,
            };
            state.acquiring.insert(record.path.clone());
            state
                .spawn(
                    Job::Provision {
                        request,
                        path: record.path.clone(),
                    },
                    WorkerInput::Provision { request, record },
                )
                .into_iter()
                .collect()
        }
        HostRequest::Attached {
            request,
            session,
            now,
        } => {
            let Some(path) = state.request_paths.remove(&request) else {
                return Vec::new();
            };
            let Some(record) = state.records.get_mut(&path) else {
                return Vec::new();
            };
            record.session = Some(session);
            record.last_used_unix = now;
            let record = record.clone();
            state.persist(record).into_iter().collect()
        }
        HostRequest::Released { session, now } => {
            let mut changed = Vec::new();
            for record in state.records.values_mut() {
                if record.session == Some(session) {
                    record.session = None;
                    record.last_used_unix = now;
                    changed.push(record.clone());
                }
            }
            changed
                .into_iter()
                .filter_map(|record| state.persist(record))
                .collect()
        }
        HostRequest::Bootstrap {
            records,
            home_directory,
        } => {
            state.home_directory = home_directory;
            for record in records {
                state.records.insert(record.path.clone(), record);
            }
            state.schedule_timer().into_iter().collect()
        }
        HostRequest::Maintenance { now } => {
            let candidates = state
                .records
                .values()
                .filter(|record| {
                    record.session.is_none()
                        && !state.acquiring.contains(&record.path)
                        && now.saturating_sub(record.last_used_unix) >= UNUSED_TTL_SECS
                })
                .cloned()
                .collect::<Vec<_>>();
            let mut commands = candidates
                .into_iter()
                .filter_map(|record| {
                    state.spawn(
                        Job::Cleanup {
                            path: record.path.clone(),
                        },
                        WorkerInput::Cleanup { record },
                    )
                })
                .collect::<Vec<_>>();
            commands.extend(state.schedule_timer());
            commands
        }
    }
}

fn handle_worker_finished(finished: WorkerFinished) -> Vec<Command> {
    let Ok(output) = serde_json::from_slice::<WorkerOutput>(&finished.output) else {
        return handle_worker_failed(finished.worker, "worker returned invalid JSON");
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let Some(job) = state.jobs.remove(&finished.worker) else {
        return Vec::new();
    };
    match job {
        Job::Provision { path, .. } => {
            state.acquiring.remove(&path);
            let Some(request) = output.request else {
                return Vec::new();
            };
            if !output.success {
                return host_message("worktree-failed", request, None, Some(&output.message));
            }
            let Some(record) = output.record else {
                return host_message(
                    "worktree-failed",
                    request,
                    None,
                    Some("provision worker returned no worktree record"),
                );
            };
            state.request_paths.insert(request, record.path.clone());
            state.records.insert(record.path.clone(), record.clone());
            host_message("worktree-provisioned", request, Some(&record.path), None)
        }
        Job::Persist => Vec::new(),
        Job::Cleanup { path } => {
            if output.success {
                state.records.remove(&path);
            }
            Vec::new()
        }
        Job::ListDirectories { request, actor } => directory_list_response(
            &actor,
            output.request.unwrap_or(request),
            output.success,
            output.directories,
            &output.message,
        ),
        Job::Timer => {
            state.timer_pending = false;
            vec![Command::SendHostMessage(
                br#"{"type":"worktree-maintenance-requested"}"#.to_vec(),
            )]
        }
    }
}

fn handle_worker_failed(worker: u64, message: &str) -> Vec<Command> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let Some(job) = state.jobs.remove(&worker) else {
        return Vec::new();
    };
    match job {
        Job::Provision { request, path } => {
            state.acquiring.remove(&path);
            host_message("worktree-failed", request, None, Some(message))
        }
        Job::ListDirectories { request, actor } => {
            directory_list_response(&actor, request, false, Vec::new(), message)
        }
        Job::Timer => {
            state.timer_pending = false;
            Vec::new()
        }
        Job::Persist | Job::Cleanup { .. } => Vec::new(),
    }
}

fn directory_list_response(
    actor: &str,
    request: u64,
    success: bool,
    directories: Vec<String>,
    message: &str,
) -> Vec<Command> {
    serde_json::to_vec(&serde_json::json!({
        "type": "directory-list",
        "request": request,
        "success": success,
        "directories": directories,
        "message": message,
    }))
    .ok()
    .map(|body| send_presentation(actor, body))
    .into_iter()
    .collect()
}

fn host_message(
    message_type: &str,
    request: u64,
    path: Option<&str>,
    message: Option<&str>,
) -> Vec<Command> {
    let value = serde_json::json!({
        "type": message_type,
        "request": request,
        "path": path,
        "message": message,
    });
    serde_json::to_vec(&value)
        .ok()
        .map(Command::SendHostMessage)
        .into_iter()
        .collect()
}

async fn provision_worktree(record: &WorktreeRecord) -> Result<(), String> {
    let parent = std::path::Path::new(&record.path)
        .parent()
        .ok_or_else(|| "managed worktree path has no parent".to_string())?
        .to_string_lossy()
        .into_owned();
    let created_parent = run_command("mkdir", &["-p".to_string(), parent]).await?;
    ensure_success(&created_parent, "create managed worktree directory")?;
    let checked = run_git(&[
        "check-ref-format".to_string(),
        "--branch".to_string(),
        record.branch.clone(),
    ])
    .await?;
    ensure_success(&checked, "validate Git branch name")?;

    let branch_ref = format!("refs/heads/{}", record.branch);
    let branch_check = run_git(&[
        "-C".to_string(),
        record.source_directory.clone(),
        "show-ref".to_string(),
        "--verify".to_string(),
        "--quiet".to_string(),
        branch_ref,
    ])
    .await?;
    let mut arguments = vec![
        "-C".to_string(),
        record.source_directory.clone(),
        "worktree".to_string(),
        "add".to_string(),
    ];
    if branch_check.status == 0 {
        arguments.push(record.path.clone());
        arguments.push(record.branch.clone());
    } else {
        arguments.extend([
            "-b".to_string(),
            record.branch.clone(),
            record.path.clone(),
            "HEAD".to_string(),
        ]);
    }
    let created = run_git(&arguments).await?;
    if created.status != 0 {
        let existing_branch = run_git(&[
            "-C".to_string(),
            record.path.clone(),
            "branch".to_string(),
            "--show-current".to_string(),
        ])
        .await?;
        let existing_project = run_git(&[
            "-C".to_string(),
            record.path.clone(),
            "rev-parse".to_string(),
            "--path-format=absolute".to_string(),
            "--git-common-dir".to_string(),
        ])
        .await?;
        let existing_branch = String::from_utf8_lossy(&existing_branch.stdout)
            .trim()
            .to_string();
        let existing_project = String::from_utf8_lossy(&existing_project.stdout)
            .trim()
            .to_string();
        if existing_branch != record.branch || existing_project != record.project_id {
            return Err(command_error(&created, "create managed Git worktree"));
        }
    }
    persist_record(record).await
}

async fn persist_record(record: &WorktreeRecord) -> Result<(), String> {
    let mut bytes =
        serde_json::to_vec(record).map_err(|error| format!("encode worktree metadata: {error}"))?;
    if bytes.len() > METADATA_BYTES {
        return Err("worktree metadata exceeds fixed record size".to_string());
    }
    bytes.resize(METADATA_BYTES, b' ');
    let written = host::worker_write_file(record.metadata_path.clone(), 0, bytes)
        .await
        .map_err(|error| format!("write worktree metadata: {error:?}"))?;
    if written == METADATA_BYTES as u64 {
        Ok(())
    } else {
        Err(format!("short worktree metadata write: {written}"))
    }
}

async fn cleanup_worktree(record: &WorktreeRecord) -> Result<(), String> {
    let status = run_git(&[
        "-C".to_string(),
        record.path.clone(),
        "status".to_string(),
        "--porcelain".to_string(),
    ])
    .await?;
    ensure_success(&status, "inspect managed worktree")?;
    if !status.stdout.is_empty() {
        return Err("managed worktree has uncommitted changes".to_string());
    }
    let removed = run_git(&[
        "-C".to_string(),
        record.source_directory.clone(),
        "worktree".to_string(),
        "remove".to_string(),
        record.path.clone(),
    ])
    .await?;
    ensure_success(&removed, "remove managed Git worktree")?;
    let metadata_removed =
        run_command("rm", &["-f".to_string(), record.metadata_path.clone()]).await?;
    ensure_success(&metadata_removed, "remove managed worktree metadata")
}

async fn list_directories(roots: &[String]) -> Result<Vec<String>, String> {
    // Presentation actor mailboxes are deliberately small. Keep the complete
    // worker result and the eventual cross-realm reply below that boundary.
    const DIRECTORY_RESULT_BYTES: usize = 12 * 1024;
    const DIRECTORY_RESULT_COUNT: usize = 256;

    let preferred_roots = roots
        .iter()
        .filter(|root| root.as_str() != "/" && root.starts_with('/'))
        .cloned()
        .collect::<Vec<_>>();
    let mut directories = BTreeSet::new();
    let mut failures = Vec::new();
    for root in roots {
        if !root.starts_with('/') {
            continue;
        }
        let depth = if root == "/" { "2" } else { "3" };
        let output = run_command(
            "find",
            &[
                root.clone(),
                "-maxdepth".to_string(),
                depth.to_string(),
                "-type".to_string(),
                "d".to_string(),
                "-print".to_string(),
            ],
        )
        .await?;
        let before = directories.len();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let path = line.trim_end_matches('/');
            if path.is_empty() || !path.starts_with('/') || directory_is_implementation_noise(path)
            {
                continue;
            }
            directories.insert(path.to_string());
        }
        if output.status != 0 && directories.len() == before {
            failures.push(command_error(&output, &format!("enumerate {root}")));
        }
    }
    if directories.is_empty() {
        return Err(if failures.is_empty() {
            "backend returned no selectable directories".to_string()
        } else {
            failures.join("; ")
        });
    }
    let mut directories = directories.into_iter().collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        let hidden_components = |path: &str| {
            path.split('/')
                .filter(|component| component.starts_with('.'))
                .count()
        };
        let depth = |path: &str| {
            path.split('/')
                .filter(|component| !component.is_empty())
                .count()
        };
        let preferred = |path: &str| {
            !preferred_roots
                .iter()
                .any(|root| path_is_within(path, root))
        };
        preferred(left)
            .cmp(&preferred(right))
            .then_with(|| hidden_components(left).cmp(&hidden_components(right)))
            .then_with(|| depth(left).cmp(&depth(right)))
            .then_with(|| left.cmp(right))
    });
    let mut selected = Vec::new();
    let mut bytes: usize = 0;
    for directory in directories {
        let next_bytes = directory.len().saturating_add(4);
        if selected.len() >= DIRECTORY_RESULT_COUNT
            || bytes.saturating_add(next_bytes) > DIRECTORY_RESULT_BYTES
        {
            break;
        }
        bytes = bytes.saturating_add(next_bytes);
        selected.push(directory);
    }
    Ok(selected)
}

fn path_is_within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn directory_is_implementation_noise(path: &str) -> bool {
    ["/dev", "/proc", "/run", "/sys", "/nix/store"]
        .iter()
        .any(|root| path_is_within(path, root))
        || path.ends_with("/.git")
        || path.contains("/.git/")
        || path.ends_with("/node_modules")
        || path.contains("/node_modules/")
        || path.ends_with("/target")
        || path.contains("/target/")
}

struct GitOutput {
    status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_git(arguments: &[String]) -> Result<GitOutput, String> {
    run_command("git", arguments).await
}

async fn run_command(command: &str, arguments: &[String]) -> Result<GitOutput, String> {
    let process = host::subprocess_spawn(command, arguments)
        .map_err(|error| format!("spawn {command}: {error:?}"))?;
    host::stream_close_write(process.stdin)
        .map_err(|error| format!("close {command} stdin: {error:?}"))?;
    let stdout = read_to_eof(process.stdout).await?;
    let stderr = read_to_eof(process.stderr).await?;
    let (state, code) = wait_child(process.child).await?;
    let _ = host::stream_drop(process.stdin).await;
    let _ = host::stream_drop(process.stdout).await;
    let _ = host::stream_drop(process.stderr).await;
    let _ = host::child_drop(process.child).await;
    let status = match state {
        host::ChildState::Exited => code.unwrap_or(1),
        host::ChildState::Signalled => 128 + code.unwrap_or(0),
        host::ChildState::Running => {
            return Err(format!("{command} remained running after wait"));
        }
    };
    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

fn ensure_success(output: &GitOutput, action: &str) -> Result<(), String> {
    if output.status == 0 {
        Ok(())
    } else {
        Err(command_error(output, action))
    }
}

fn command_error(output: &GitOutput, action: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        format!("{action} failed with status {}", output.status)
    } else {
        format!("{action} failed: {detail}")
    }
}

async fn read_to_eof(stream: u64) -> Result<Vec<u8>, String> {
    let notification = host::stream_notification(stream)
        .map_err(|error| format!("stream notification: {error:?}"))?;
    let mut generation = 0;
    let mut output = Vec::new();
    loop {
        match host::stream_read(stream, 4096).map_err(|error| format!("stream read: {error:?}"))? {
            host::StreamReadResult::Data(bytes) => output.extend(bytes),
            host::StreamReadResult::Eof => return Ok(output),
            host::StreamReadResult::WouldBlock => {
                generation = wait_notification(notification, generation).await?;
            }
        }
    }
}

bindings::export!(WorktreeManager with_types_in bindings);
