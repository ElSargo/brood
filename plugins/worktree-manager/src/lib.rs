wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use brood::plugin::host;
use exports::brood::plugin::actor::{
    Command, Event, Guest, LifecycleEvent, WorkerEvent, WorkerFinished, WorkerRequest,
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
    Timer,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkerOutput {
    success: bool,
    message: String,
    request: Option<u64>,
    record: Option<WorktreeRecord>,
}

#[derive(Clone, Debug)]
enum Job {
    Provision { request: u64, path: String },
    Persist,
    Cleanup { path: String },
    Timer,
}

struct ManagerState {
    next_worker: u64,
    jobs: BTreeMap<u64, Job>,
    records: BTreeMap<String, WorktreeRecord>,
    request_paths: BTreeMap<u64, String>,
    acquiring: BTreeSet<String>,
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
            timer_pending: false,
        }
    }

    fn spawn(&mut self, job: Job, input: WorkerInput) -> Option<Command> {
        let worker = self.next_worker;
        self.next_worker = self.next_worker.saturating_add(1);
        let payload = serde_json::to_vec(&input).ok()?;
        self.jobs.insert(worker, job);
        Some(Command::SpawnWorker(WorkerRequest { worker, payload }))
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
                },
                Err(message) => WorkerOutput {
                    success: false,
                    message,
                    request: Some(request),
                    record: None,
                },
            },
            WorkerInput::Persist { record } => WorkerOutput {
                success: persist_record(&record).await.is_ok(),
                message: String::new(),
                request: None,
                record: Some(record),
            },
            WorkerInput::Cleanup { record } => match cleanup_worktree(&record).await {
                Ok(()) => WorkerOutput {
                    success: true,
                    message: String::new(),
                    request: None,
                    record: Some(record),
                },
                Err(message) => WorkerOutput {
                    success: false,
                    message,
                    request: None,
                    record: Some(record),
                },
            },
            WorkerInput::Timer => {
                wait_timer(MAINTENANCE_INTERVAL_MS).await?;
                WorkerOutput {
                    success: true,
                    message: String::new(),
                    request: None,
                    record: None,
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
    let authorized = matches!(
        (&request, sender),
        (HostRequest::Acquire { .. }, "host:control")
            | (
                HostRequest::Attached { .. } | HostRequest::Released { .. },
                "host:session"
            )
            | (HostRequest::Bootstrap { .. }, "host")
            | (HostRequest::Maintenance { .. }, "host:maintenance")
    );
    if !authorized {
        return Vec::new();
    }
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match request {
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
        HostRequest::Bootstrap { records } => {
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
        Job::Timer => {
            state.timer_pending = false;
            Vec::new()
        }
        Job::Persist | Job::Cleanup { .. } => Vec::new(),
    }
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

async fn wait_child(child: u64) -> Result<(host::ChildState, Option<i32>), String> {
    let notification = host::child_notification(child)
        .map_err(|error| format!("child notification: {error:?}"))?;
    let mut generation = 0;
    loop {
        let status =
            host::child_status(child).map_err(|error| format!("child status: {error:?}"))?;
        if status.0 != host::ChildState::Running {
            return Ok(status);
        }
        generation = wait_notification(notification, generation).await?;
    }
}

async fn wait_timer(duration_ms: u64) -> Result<(), String> {
    let timer =
        host::timer_create(duration_ms).map_err(|error| format!("create timer: {error:?}"))?;
    let notification = host::timer_notification(timer)
        .map_err(|error| format!("timer notification: {error:?}"))?;
    let mut generation = 0;
    while host::timer_status(timer).map_err(|error| format!("timer status: {error:?}"))?
        == host::TimerState::Pending
    {
        generation = wait_notification(notification, generation).await?;
    }
    Ok(())
}

async fn wait_notification(notification: u64, generation: u64) -> Result<u64, String> {
    let pollable = host::notification_pollable(notification, generation)
        .map_err(|error| format!("create pollable: {error:?}"))?;
    let ready = host::worker_wait_any(vec![pollable], None, host::WaitMode::Fair)
        .await
        .map_err(|error| format!("wait notification: {error:?}"));
    let _ = host::pollable_drop(pollable);
    ready.map(|ready| ready.generation)
}

export!(WorktreeManager);
