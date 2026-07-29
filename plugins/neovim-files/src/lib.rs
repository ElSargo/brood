wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use brood::plugin::host;
use exports::brood::plugin::actor::{
    Command, Event, Guest, LifecycleEvent, MailboxMessage, PresentationActorMessage, WorkerEvent,
    WorkerFailed, WorkerFinished, WorkerRequest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

const PROTOCOL_VERSION: u32 = 1;
const MAX_WIRE_BYTES: usize = 15 * 1024;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_ACTOR_BYTES: usize = 128;
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHUNK_BYTES: usize = 7 * 1024;
const MAX_OPEN_FILES: usize = 4;
const MAX_TRANSFERS: usize = 4;
const MAX_ACTIVE_JOBS: usize = 8;
const READ_BLOCK_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 240;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Request {
    Hello {
        request: u64,
    },
    Open {
        request: u64,
        path: String,
    },
    Read {
        request: u64,
        path: String,
        offset: u64,
    },
    OpenAbort {
        request: u64,
    },
    SaveBegin {
        request: u64,
        transfer: u64,
        path: String,
        base_revision: String,
        size: u64,
    },
    SaveChunk {
        request: u64,
        transfer: u64,
        offset: u64,
        data_hex: String,
    },
    SaveCommit {
        request: u64,
        transfer: u64,
    },
    SaveAbort {
        request: u64,
        transfer: u64,
    },
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Control {
    BindClient { session_id: u64, actor: String },
    RevokeClient { session_id: u64, actor: String },
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum ControlResponse {
    EditorClientAuthorized {
        session_id: u64,
        actor: String,
        version: u32,
    },
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Response {
    Hello {
        request: u64,
        version: u32,
        max_file_bytes: u64,
        max_chunk_bytes: u64,
        max_open_files: u64,
        max_transfers: u64,
    },
    Open {
        request: u64,
        path: String,
        revision: String,
        size: u64,
        offset: u64,
        data_hex: String,
        eof: bool,
    },
    Read {
        request: u64,
        path: String,
        revision: String,
        size: u64,
        offset: u64,
        data_hex: String,
        eof: bool,
    },
    SaveReady {
        request: u64,
        transfer: u64,
        path: String,
        size: u64,
    },
    SaveChunkAccepted {
        request: u64,
        transfer: u64,
        next_offset: u64,
    },
    SaveComplete {
        request: u64,
        transfer: u64,
        path: String,
        revision: String,
        size: u64,
    },
    Error {
        request: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        transfer: Option<u64>,
        code: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_revision: Option<String>,
    },
}

impl Response {
    const fn request(&self) -> u64 {
        match self {
            Self::Hello { request, .. }
            | Self::Open { request, .. }
            | Self::Read { request, .. }
            | Self::SaveReady { request, .. }
            | Self::SaveChunkAccepted { request, .. }
            | Self::SaveComplete { request, .. }
            | Self::Error { request, .. } => *request,
        }
    }

    const fn transfer(&self) -> Option<u64> {
        match self {
            Self::SaveReady { transfer, .. }
            | Self::SaveChunkAccepted { transfer, .. }
            | Self::SaveComplete { transfer, .. } => Some(*transfer),
            Self::Error { transfer, .. } => *transfer,
            Self::Hello { .. } | Self::Open { .. } | Self::Read { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum WorkerInput {
    Open { path: String },
    Save { actor: String, transfer: u64 },
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum WorkerOutput {
    Opened { revision: String, size: u64 },
    Saved { revision: String, size: u64 },
    Conflict { current_revision: String },
    Error { code: String, message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FileKey {
    actor: String,
    request: u64,
    path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TransferKey {
    actor: String,
    transfer: u64,
}

#[derive(Debug)]
struct Snapshot {
    revision: String,
    bytes: Vec<u8>,
    next_offset: usize,
}

#[derive(Debug)]
struct SaveTransfer {
    request: u64,
    path: String,
    base_revision: String,
    declared_size: usize,
    bytes: Vec<u8>,
    committing: bool,
}

impl SaveTransfer {
    fn append(&mut self, offset: u64, data_hex: &str) -> Result<u64, ProtocolError> {
        if self.committing {
            return Err(ProtocolError::new(
                "transfer-committing",
                "save transfer is already committing",
            ));
        }
        if data_hex.len() > MAX_CHUNK_BYTES.saturating_mul(2) {
            return Err(ProtocolError::new(
                "chunk-too-large",
                "save chunk exceeds the negotiated limit",
            ));
        }
        let expected = u64::try_from(self.bytes.len()).map_err(|_overflow| {
            ProtocolError::new("invalid-offset", "save transfer offset overflowed")
        })?;
        if offset != expected {
            return Err(ProtocolError::new(
                "invalid-offset",
                "save chunks must be contiguous and in order",
            ));
        }
        let decoded = decode_hex(data_hex)?;
        let next_size = self
            .bytes
            .len()
            .checked_add(decoded.len())
            .ok_or_else(|| ProtocolError::new("file-too-large", "save size overflowed"))?;
        if next_size > self.declared_size || next_size > MAX_FILE_BYTES {
            return Err(ProtocolError::new(
                "file-too-large",
                "save chunks exceed the declared file size",
            ));
        }
        self.bytes.extend_from_slice(&decoded);
        u64::try_from(self.bytes.len())
            .map_err(|_overflow| ProtocolError::new("file-too-large", "save size overflowed"))
    }
}

#[derive(Debug)]
enum Job {
    Open {
        actor: String,
        path: String,
        request: u64,
    },
    Save {
        actor: String,
        path: String,
        request: u64,
        transfer: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ClientBinding {
    session_id: u64,
    actor: String,
}

struct State {
    next_worker: u64,
    client: Option<ClientBinding>,
    jobs: BTreeMap<u64, Job>,
    suppressed_jobs: BTreeSet<u64>,
    snapshots: BTreeMap<FileKey, Snapshot>,
    completed_snapshots: BTreeMap<u64, Snapshot>,
    transfers: BTreeMap<TransferKey, SaveTransfer>,
    path_locks: BTreeMap<String, TransferKey>,
}

impl State {
    const fn new() -> Self {
        Self {
            next_worker: 1,
            client: None,
            jobs: BTreeMap::new(),
            suppressed_jobs: BTreeSet::new(),
            snapshots: BTreeMap::new(),
            completed_snapshots: BTreeMap::new(),
            transfers: BTreeMap::new(),
            path_locks: BTreeMap::new(),
        }
    }

    fn allocate_worker(&mut self) -> Result<u64, ProtocolError> {
        if self.jobs.len() >= MAX_ACTIVE_JOBS {
            return Err(ProtocolError::new(
                "limit-reached",
                "file worker limit reached",
            ));
        }
        let worker = self.next_worker;
        self.next_worker = self
            .next_worker
            .checked_add(1)
            .ok_or_else(|| ProtocolError::new("limit-reached", "file worker IDs are exhausted"))?;
        Ok(worker)
    }

    fn open_request_active(&self, actor: &str, request: u64) -> bool {
        self.snapshots
            .keys()
            .any(|key| key.actor == actor && key.request == request)
            || self.jobs.values().any(|job| {
                matches!(
                    job,
                    Job::Open {
                        actor: job_actor,
                        request: job_request,
                        ..
                    } if job_actor == actor && *job_request == request
                )
            })
    }

    fn open_slots(&self) -> usize {
        self.snapshots.len().saturating_add(
            self.jobs
                .values()
                .filter(|job| matches!(job, Job::Open { .. }))
                .count(),
        )
    }

    fn transfer_slots(&self) -> usize {
        let committing = self
            .jobs
            .values()
            .filter(|job| {
                let Job::Save {
                    actor, transfer, ..
                } = job
                else {
                    return false;
                };
                !self.transfers.contains_key(&TransferKey {
                    actor: actor.clone(),
                    transfer: *transfer,
                })
            })
            .count();
        self.transfers.len().saturating_add(committing)
    }

    fn transfer_key_active(&self, key: &TransferKey) -> bool {
        self.transfers.contains_key(key)
            || self.jobs.values().any(|job| {
                matches!(
                    job,
                    Job::Save {
                        actor,
                        transfer,
                        ..
                    } if actor == &key.actor && transfer == &key.transfer
                )
            })
    }

    fn client_is_authorized(&self, actor: &str) -> bool {
        self.client
            .as_ref()
            .is_some_and(|client| client.actor == actor)
    }

    fn snapshot_key(&self, actor: &str, request: u64) -> Option<FileKey> {
        self.snapshots
            .keys()
            .find(|key| key.actor == actor && key.request == request)
            .cloned()
    }

    fn abort_snapshot(&mut self, actor: &str, request: u64) {
        if let Some(key) = self.snapshot_key(actor, request) {
            self.snapshots.remove(&key);
        }
    }

    fn abort_open_request(&mut self, actor: &str, request: u64) -> Vec<u64> {
        self.abort_snapshot(actor, request);
        let workers = self
            .jobs
            .iter()
            .filter_map(|(worker, job)| {
                matches!(
                    job,
                    Job::Open {
                        actor: job_actor,
                        request: job_request,
                        ..
                    } if job_actor == actor && *job_request == request
                )
                .then_some(*worker)
            })
            .collect::<Vec<_>>();
        self.suppressed_jobs.extend(workers.iter().copied());
        workers
    }

    fn abort_transfer(&mut self, actor: &str, transfer: u64) {
        self.transfers.remove(&TransferKey {
            actor: actor.to_string(),
            transfer,
        });
    }

    fn abort_save_request(&mut self, actor: &str, request: u64, transfer: u64) -> Vec<u64> {
        let key = TransferKey {
            actor: actor.to_string(),
            transfer,
        };
        if self
            .transfers
            .get(&key)
            .is_some_and(|save| save.request == request)
        {
            self.transfers.remove(&key);
        }
        let workers = self
            .jobs
            .iter()
            .filter_map(|(worker, job)| {
                matches!(
                    job,
                    Job::Save {
                        actor: job_actor,
                        request: job_request,
                        transfer: job_transfer,
                        ..
                    } if job_actor == actor
                        && *job_request == request
                        && *job_transfer == transfer
                )
                .then_some(*worker)
            })
            .collect::<Vec<_>>();
        self.suppressed_jobs.extend(workers.iter().copied());
        workers
    }

    fn suppress_actor_jobs(&mut self, actor: &str) -> Vec<u64> {
        self.snapshots.retain(|key, _snapshot| key.actor != actor);
        self.transfers.retain(|key, _save| key.actor != actor);
        let workers = self
            .jobs
            .iter()
            .filter_map(|(worker, job)| (job_actor(job) == actor).then_some(*worker))
            .collect::<Vec<_>>();
        self.suppressed_jobs.extend(workers.iter().copied());
        workers
    }

    fn release_save_job(&mut self, actor: &str, path: &str, transfer: u64) {
        let key = TransferKey {
            actor: actor.to_string(),
            transfer,
        };
        self.transfers.remove(&key);
        if self.path_locks.get(path) == Some(&key) {
            self.path_locks.remove(path);
        }
    }
}

static STATE: Mutex<State> = Mutex::new(State::new());

#[derive(Debug, PartialEq, Eq)]
struct ProtocolError {
    code: &'static str,
    message: &'static str,
}

impl ProtocolError {
    const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }
}

struct NeovimFiles;

impl Guest for NeovimFiles {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(_)) => {
                if let Ok(mut state) = STATE.lock() {
                    *state = State::new();
                }
                vec![Command::LifecycleComplete(Ok(()))]
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                let mut commands = STATE
                    .lock()
                    .ok()
                    .and_then(|mut state| {
                        let actor = state.client.take()?.actor;
                        Some(cancel_workers(state.suppress_actor_jobs(&actor)))
                    })
                    .unwrap_or_default();
                commands.push(Command::LifecycleComplete(Ok(())));
                commands
            }
            Event::Mailbox(message) => handle_mailbox(message),
            Event::Worker(WorkerEvent::Finished(finished)) => handle_worker_finished(finished),
            Event::Worker(WorkerEvent::Failed(failed)) => handle_worker_failed(failed),
            Event::Worker(WorkerEvent::Cancelled(worker)) => handle_worker_cancelled(worker),
            _ => Vec::new(),
        }
    }

    async fn worker_run(worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        let input: WorkerInput = serde_json::from_slice(&payload)
            .map_err(|error| format!("decode file job: {error}"))?;
        let output = match input {
            WorkerInput::Open { path } => open_file(worker, &path).await,
            WorkerInput::Save { actor, transfer } => save_file(&actor, transfer).await,
        };
        serde_json::to_vec(&output).map_err(|error| format!("encode file job result: {error}"))
    }
}

fn handle_mailbox(message: MailboxMessage) -> Vec<Command> {
    if message.sender == "init" {
        return handle_control(&message.body);
    }
    let Some(actor) = presentation_actor(&message.sender) else {
        return Vec::new();
    };
    let authorized = STATE
        .lock()
        .ok()
        .is_some_and(|state| state.client_is_authorized(&actor));
    if !authorized {
        return error_reply(
            &actor,
            0,
            None,
            "unauthorized",
            "session init has not authorized this presentation actor",
            None,
        );
    }
    if message.body.len() > MAX_WIRE_BYTES {
        let hint = request_hint(&message.body);
        abort_hinted_request(&actor, &hint);
        return reply(
            &actor,
            error_response(
                hint.request,
                hint.transfer,
                "message-too-large",
                "request exceeds the cross-placement message limit",
                None,
            ),
        );
    }
    let request = match serde_json::from_slice::<Request>(&message.body) {
        Ok(request) => request,
        Err(error) => {
            let hint = request_hint(&message.body);
            abort_hinted_request(&actor, &hint);
            return reply(
                &actor,
                error_response(
                    hint.request,
                    hint.transfer,
                    "invalid-request",
                    &format!("request is not valid protocol JSON: {error}"),
                    None,
                ),
            );
        }
    };
    handle_request(&actor, request)
}

fn handle_control(body: &[u8]) -> Vec<Command> {
    if body.len() > MAX_WIRE_BYTES {
        return Vec::new();
    }
    let Ok(control) = serde_json::from_slice::<Control>(body) else {
        return Vec::new();
    };
    match control {
        Control::BindClient { session_id, actor }
            if session_id != 0 && valid_presentation_actor(&actor) =>
        {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            let mut cancelled = BTreeSet::new();
            if let Some(previous) = state.client.take() {
                cancelled.extend(state.suppress_actor_jobs(&previous.actor));
            }
            cancelled.extend(state.suppress_actor_jobs(&actor));
            state.client = Some(ClientBinding {
                session_id,
                actor: actor.clone(),
            });
            drop(state);
            let mut commands = cancel_workers(cancelled);
            commands.extend(control_reply(
                &actor,
                ControlResponse::EditorClientAuthorized {
                    session_id,
                    actor: actor.clone(),
                    version: PROTOCOL_VERSION,
                },
            ));
            commands
        }
        Control::RevokeClient { session_id, actor } => {
            let Ok(mut state) = STATE.lock() else {
                return Vec::new();
            };
            if state.client.as_ref()
                != Some(&ClientBinding {
                    session_id,
                    actor: actor.clone(),
                })
            {
                return Vec::new();
            }
            state.client = None;
            let workers = state.suppress_actor_jobs(&actor);
            drop(state);
            cancel_workers(workers)
        }
        Control::BindClient { .. } => Vec::new(),
    }
}

#[derive(Default)]
struct RequestHint {
    kind: Option<String>,
    request: u64,
    transfer: Option<u64>,
}

fn request_hint(body: &[u8]) -> RequestHint {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return RequestHint::default();
    };
    RequestHint {
        kind: value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        request: value
            .get("request")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        transfer: value.get("transfer").and_then(serde_json::Value::as_u64),
    }
}

fn abort_hinted_request(actor: &str, hint: &RequestHint) {
    let Ok(mut state) = STATE.lock() else {
        return;
    };
    match hint.kind.as_deref() {
        Some("read" | "open-abort") => state.abort_snapshot(actor, hint.request),
        Some("save-begin" | "save-chunk" | "save-commit" | "save-abort") => {
            if let Some(transfer) = hint.transfer {
                state.abort_transfer(actor, transfer);
            }
        }
        _ => {}
    }
}

fn cancel_workers(workers: impl IntoIterator<Item = u64>) -> Vec<Command> {
    workers.into_iter().map(Command::CancelWorker).collect()
}

fn handle_request(actor: &str, request: Request) -> Vec<Command> {
    match request {
        Request::Hello { request } => reply(
            actor,
            Response::Hello {
                request,
                version: PROTOCOL_VERSION,
                max_file_bytes: MAX_FILE_BYTES as u64,
                max_chunk_bytes: MAX_CHUNK_BYTES as u64,
                max_open_files: MAX_OPEN_FILES as u64,
                max_transfers: MAX_TRANSFERS as u64,
            },
        ),
        Request::Open { request, path } => start_open(actor, request, &path),
        Request::Read {
            request,
            path,
            offset,
        } => read_snapshot(actor, request, &path, offset),
        Request::OpenAbort { request } => abort_open_request(actor, request),
        Request::SaveBegin {
            request,
            transfer,
            path,
            base_revision,
            size,
        } => start_save(actor, request, transfer, &path, &base_revision, size),
        Request::SaveChunk {
            request,
            transfer,
            offset,
            data_hex,
        } => append_save(actor, request, transfer, offset, &data_hex),
        Request::SaveCommit { request, transfer } => commit_save(actor, request, transfer),
        Request::SaveAbort { request, transfer } => abort_save_request(actor, request, transfer),
    }
}

fn start_open(actor: &str, request: u64, path: &str) -> Vec<Command> {
    let path = match normalize_path(path) {
        Ok(path) => path,
        Err(error) => return protocol_error(actor, request, None, error),
    };
    let Ok(mut state) = STATE.lock() else {
        return internal_error(actor, request, None);
    };
    if state.open_request_active(actor, request) {
        return error_reply(
            actor,
            request,
            None,
            "duplicate-request",
            "open request ID is already active",
            None,
        );
    }
    if state.open_slots() >= MAX_OPEN_FILES {
        return error_reply(
            actor,
            request,
            None,
            "limit-reached",
            "open file limit reached",
            None,
        );
    }
    let worker = match state.allocate_worker() {
        Ok(worker) => worker,
        Err(error) => return protocol_error(actor, request, None, error),
    };
    let payload = match serde_json::to_vec(&WorkerInput::Open { path: path.clone() }) {
        Ok(payload) => payload,
        Err(_error) => return internal_error(actor, request, None),
    };
    state.jobs.insert(
        worker,
        Job::Open {
            actor: actor.to_string(),
            path,
            request,
        },
    );
    vec![Command::SpawnWorker(WorkerRequest { worker, payload })]
}

fn read_snapshot(actor: &str, request: u64, path: &str, offset: u64) -> Vec<Command> {
    let path = match normalize_path(path) {
        Ok(path) => path,
        Err(error) => {
            abort_snapshot(actor, request);
            return protocol_error(actor, request, None, error);
        }
    };
    let Ok(mut state) = STATE.lock() else {
        return internal_error(actor, request, None);
    };
    let Some(key) = state.snapshot_key(actor, request) else {
        return error_reply(
            actor,
            request,
            None,
            "not-open",
            "open the file before requesting more chunks",
            None,
        );
    };
    if key.path != path {
        state.snapshots.remove(&key);
        drop(state);
        return error_reply(
            actor,
            request,
            None,
            "path-mismatch",
            "read path does not match the opened file",
            None,
        );
    }
    let Some(snapshot) = state.snapshots.get(&key) else {
        return error_reply(
            actor,
            request,
            None,
            "not-open",
            "open the file before requesting more chunks",
            None,
        );
    };
    if usize::try_from(offset).ok() != Some(snapshot.next_offset) {
        state.snapshots.remove(&key);
        drop(state);
        return error_reply(
            actor,
            request,
            None,
            "invalid-offset",
            "read chunks must be requested contiguously and in order",
            None,
        );
    }
    let response = match file_chunk_response(false, request, path, snapshot, offset) {
        Ok(response) => response,
        Err(error) => {
            state.snapshots.remove(&key);
            drop(state);
            return protocol_error(actor, request, None, error);
        }
    };
    if response_eof(&response) {
        state.snapshots.remove(&key);
    } else if let Some(snapshot) = state.snapshots.get_mut(&key) {
        snapshot.next_offset = response_next_offset(&response);
    }
    drop(state);
    reply(actor, response)
}

fn abort_snapshot(actor: &str, request: u64) {
    if let Ok(mut state) = STATE.lock() {
        state.abort_snapshot(actor, request);
    }
}

fn abort_open_request(actor: &str, request: u64) -> Vec<Command> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let workers = state.abort_open_request(actor, request);
    drop(state);
    cancel_workers(workers)
}

fn abort_transfer(actor: &str, transfer: u64) {
    if let Ok(mut state) = STATE.lock() {
        state.abort_transfer(actor, transfer);
    }
}

fn abort_save_request(actor: &str, request: u64, transfer: u64) -> Vec<Command> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let workers = state.abort_save_request(actor, request, transfer);
    drop(state);
    cancel_workers(workers)
}

fn start_save(
    actor: &str,
    request: u64,
    transfer: u64,
    path: &str,
    base_revision: &str,
    size: u64,
) -> Vec<Command> {
    if transfer == 0 {
        return error_reply(
            actor,
            request,
            Some(transfer),
            "invalid-transfer",
            "transfer ID must be non-zero",
            None,
        );
    }
    let path = match normalize_path(path) {
        Ok(path) => path,
        Err(error) => {
            abort_transfer(actor, transfer);
            return protocol_error(actor, request, Some(transfer), error);
        }
    };
    let base_revision = match normalize_revision(base_revision) {
        Ok(revision) => revision,
        Err(error) => {
            abort_transfer(actor, transfer);
            return protocol_error(actor, request, Some(transfer), error);
        }
    };
    let Ok(declared_size) = usize::try_from(size) else {
        abort_transfer(actor, transfer);
        return error_reply(
            actor,
            request,
            Some(transfer),
            "file-too-large",
            "declared file size exceeds the negotiated limit",
            None,
        );
    };
    if declared_size > MAX_FILE_BYTES {
        abort_transfer(actor, transfer);
        return error_reply(
            actor,
            request,
            Some(transfer),
            "file-too-large",
            "declared file size exceeds the negotiated limit",
            None,
        );
    }

    let key = TransferKey {
        actor: actor.to_string(),
        transfer,
    };
    let Ok(mut state) = STATE.lock() else {
        return internal_error(actor, request, Some(transfer));
    };
    if state.transfer_key_active(&key) {
        state.abort_transfer(actor, transfer);
        drop(state);
        return error_reply(
            actor,
            request,
            Some(transfer),
            "invalid-transfer",
            "transfer ID is already active",
            None,
        );
    }
    if state.transfer_slots() >= MAX_TRANSFERS {
        return error_reply(
            actor,
            request,
            Some(transfer),
            "limit-reached",
            "save transfer limit reached",
            None,
        );
    }
    state.transfers.insert(
        key,
        SaveTransfer {
            request,
            path: path.clone(),
            base_revision,
            declared_size,
            bytes: Vec::new(),
            committing: false,
        },
    );
    drop(state);
    reply(
        actor,
        Response::SaveReady {
            request,
            transfer,
            path,
            size,
        },
    )
}

fn append_save(
    actor: &str,
    request: u64,
    transfer: u64,
    offset: u64,
    data_hex: &str,
) -> Vec<Command> {
    let key = TransferKey {
        actor: actor.to_string(),
        transfer,
    };
    let Ok(mut state) = STATE.lock() else {
        return internal_error(actor, request, Some(transfer));
    };
    let Some(save) = state.transfers.get_mut(&key) else {
        return error_reply(
            actor,
            request,
            Some(transfer),
            "invalid-transfer",
            "save transfer is not active",
            None,
        );
    };
    let append_result = if save.request == request {
        save.append(offset, data_hex)
    } else {
        Err(ProtocolError::new(
            "invalid-request",
            "save request ID changed during the transfer",
        ))
    };
    let next_offset = match append_result {
        Ok(next_offset) => next_offset,
        Err(error) => {
            state.transfers.remove(&key);
            drop(state);
            return protocol_error(actor, request, Some(transfer), error);
        }
    };
    drop(state);
    reply(
        actor,
        Response::SaveChunkAccepted {
            request,
            transfer,
            next_offset,
        },
    )
}

fn commit_save(actor: &str, request: u64, transfer: u64) -> Vec<Command> {
    let key = TransferKey {
        actor: actor.to_string(),
        transfer,
    };
    let Ok(mut state) = STATE.lock() else {
        return internal_error(actor, request, Some(transfer));
    };
    let Some(save) = state.transfers.get(&key) else {
        return error_reply(
            actor,
            request,
            Some(transfer),
            "invalid-transfer",
            "save transfer is not active",
            None,
        );
    };
    if save.committing {
        return error_reply(
            actor,
            request,
            Some(transfer),
            "transfer-committing",
            "save transfer is already committing",
            None,
        );
    }
    if save.request != request {
        state.transfers.remove(&key);
        drop(state);
        return error_reply(
            actor,
            request,
            Some(transfer),
            "invalid-request",
            "save request ID changed during the transfer",
            None,
        );
    }
    if save.bytes.len() != save.declared_size {
        state.transfers.remove(&key);
        drop(state);
        return error_reply(
            actor,
            request,
            Some(transfer),
            "incomplete-save",
            "save transfer does not contain the declared number of bytes",
            None,
        );
    }
    let path = save.path.clone();
    if state.path_locks.contains_key(&path) {
        state.transfers.remove(&key);
        drop(state);
        return error_reply(
            actor,
            request,
            Some(transfer),
            "path-busy",
            "another save for this file is committing",
            None,
        );
    }
    let worker = match state.allocate_worker() {
        Ok(worker) => worker,
        Err(error) => {
            state.transfers.remove(&key);
            drop(state);
            return protocol_error(actor, request, Some(transfer), error);
        }
    };
    let payload = match serde_json::to_vec(&WorkerInput::Save {
        actor: actor.to_string(),
        transfer,
    }) {
        Ok(payload) => payload,
        Err(_error) => {
            state.transfers.remove(&key);
            drop(state);
            return internal_error(actor, request, Some(transfer));
        }
    };
    if let Some(save) = state.transfers.get_mut(&key) {
        save.committing = true;
    }
    state.path_locks.insert(path.clone(), key);
    state.jobs.insert(
        worker,
        Job::Save {
            actor: actor.to_string(),
            path,
            request,
            transfer,
        },
    );
    vec![Command::SpawnWorker(WorkerRequest { worker, payload })]
}

async fn open_file(worker: u64, path: &str) -> WorkerOutput {
    match read_file(path).await {
        Ok(bytes) => {
            let revision = revision(&bytes);
            let size = bytes.len() as u64;
            let snapshot = Snapshot {
                revision: revision.clone(),
                bytes,
                next_offset: 0,
            };
            let Ok(mut state) = STATE.lock() else {
                return WorkerOutput::Error {
                    code: "internal".to_string(),
                    message: "file state lock is unavailable".to_string(),
                };
            };
            state.completed_snapshots.insert(worker, snapshot);
            WorkerOutput::Opened { revision, size }
        }
        Err(problem) => problem.into_output(),
    }
}

async fn save_file(actor: &str, transfer: u64) -> WorkerOutput {
    let key = TransferKey {
        actor: actor.to_string(),
        transfer,
    };
    let save = {
        let Ok(mut state) = STATE.lock() else {
            return WorkerOutput::Error {
                code: "internal".to_string(),
                message: "file state lock is unavailable".to_string(),
            };
        };
        state.transfers.remove(&key)
    };
    let Some(save) = save else {
        return WorkerOutput::Error {
            code: "invalid-transfer".to_string(),
            message: "save transfer disappeared before commit".to_string(),
        };
    };
    let current = match read_file(&save.path).await {
        Ok(current) => current,
        Err(problem) => return problem.into_output(),
    };
    let current_revision = revision(&current);
    if current_revision != save.base_revision {
        return WorkerOutput::Conflict { current_revision };
    }
    let size = save.bytes.len() as u64;
    let new_revision = revision(&save.bytes);
    match host::worker_replace_file(save.path, save.bytes).await {
        Ok(written) if written == size => WorkerOutput::Saved {
            revision: new_revision,
            size,
        },
        Ok(written) => WorkerOutput::Error {
            code: "short-write".to_string(),
            message: format!("atomic replacement reported {written} of {size} bytes"),
        },
        Err(error) => WorkerOutput::Error {
            code: "io".to_string(),
            message: format!("atomic replacement failed: {error:?}"),
        },
    }
}

async fn read_file(path: &str) -> Result<Vec<u8>, WorkerProblem> {
    let mut bytes = Vec::new();
    loop {
        let offset = bytes.len() as u64;
        let chunk =
            host::worker_read_regular_file(path.to_string(), offset, READ_BLOCK_BYTES as u64)
                .await
                .map_err(|error| WorkerProblem {
                    code: "io",
                    message: format!("regular file read failed: {error:?}"),
                })?;
        if chunk.is_empty() {
            return Ok(bytes);
        }
        let next_size = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| WorkerProblem {
                code: "file-too-large",
                message: "file size overflowed".to_string(),
            })?;
        if next_size > MAX_FILE_BYTES {
            return Err(WorkerProblem {
                code: "file-too-large",
                message: format!("file exceeds the {MAX_FILE_BYTES} byte limit"),
            });
        }
        bytes.extend_from_slice(&chunk);
    }
}

struct WorkerProblem {
    code: &'static str,
    message: String,
}

impl WorkerProblem {
    fn into_output(self) -> WorkerOutput {
        WorkerOutput::Error {
            code: self.code.to_string(),
            message: self.message,
        }
    }
}

fn handle_worker_finished(finished: WorkerFinished) -> Vec<Command> {
    let output = match serde_json::from_slice::<WorkerOutput>(&finished.output) {
        Ok(output) => output,
        Err(error) => {
            return finish_failed(
                finished.worker,
                "invalid-worker-result",
                &format!("file worker returned invalid JSON: {error}"),
            );
        }
    };
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    let Some(job) = state.jobs.remove(&finished.worker) else {
        state.suppressed_jobs.remove(&finished.worker);
        state.completed_snapshots.remove(&finished.worker);
        return Vec::new();
    };
    let suppressed = state.suppressed_jobs.remove(&finished.worker);
    if let Job::Save {
        actor,
        path,
        transfer,
        ..
    } = &job
    {
        state.release_save_job(actor, path, *transfer);
    }
    if suppressed {
        state.completed_snapshots.remove(&finished.worker);
        return Vec::new();
    }
    match (job, output) {
        (
            Job::Open {
                actor,
                path,
                request,
            },
            WorkerOutput::Opened { revision, size },
        ) => {
            let Some(snapshot) = state.completed_snapshots.remove(&finished.worker) else {
                drop(state);
                return error_reply(
                    &actor,
                    request,
                    None,
                    "invalid-worker-result",
                    "open worker returned no file snapshot",
                    None,
                );
            };
            if snapshot.revision != revision || snapshot.bytes.len() as u64 != size {
                drop(state);
                return error_reply(
                    &actor,
                    request,
                    None,
                    "invalid-worker-result",
                    "open worker metadata did not match its file snapshot",
                    None,
                );
            }
            let key = FileKey {
                actor: actor.clone(),
                request,
                path: path.clone(),
            };
            state.snapshots.insert(key.clone(), snapshot);
            let response = state
                .snapshots
                .get(&key)
                .ok_or_else(|| {
                    ProtocolError::new("internal", "opened file snapshot is unavailable")
                })
                .and_then(|snapshot| file_chunk_response(true, request, path, snapshot, 0));
            if response.as_ref().is_ok_and(response_eof) {
                state.snapshots.remove(&key);
            } else if let Ok(response) = response.as_ref()
                && let Some(snapshot) = state.snapshots.get_mut(&key)
            {
                snapshot.next_offset = response_next_offset(response);
            }
            drop(state);
            match response {
                Ok(response) => reply(&actor, response),
                Err(error) => protocol_error(&actor, request, None, error),
            }
        }
        (
            Job::Save {
                actor,
                path,
                request,
                transfer,
            },
            WorkerOutput::Saved { revision, size },
        ) => {
            drop(state);
            reply(
                &actor,
                Response::SaveComplete {
                    request,
                    transfer,
                    path,
                    revision,
                    size,
                },
            )
        }
        (
            Job::Save {
                actor,
                request,
                transfer,
                ..
            },
            WorkerOutput::Conflict { current_revision },
        ) => {
            drop(state);
            error_reply(
                &actor,
                request,
                Some(transfer),
                "conflict",
                "file changed on the backend after it was opened",
                Some(current_revision),
            )
        }
        (job, WorkerOutput::Error { code, message }) => {
            state.completed_snapshots.remove(&finished.worker);
            let (actor, request, transfer) = job_response(&job);
            drop(state);
            error_reply(&actor, request, transfer, &code, &message, None)
        }
        (
            job,
            WorkerOutput::Opened { .. }
            | WorkerOutput::Saved { .. }
            | WorkerOutput::Conflict { .. },
        ) => {
            state.completed_snapshots.remove(&finished.worker);
            let (actor, request, transfer) = job_response(&job);
            drop(state);
            error_reply(
                &actor,
                request,
                transfer,
                "invalid-worker-result",
                "file worker returned a result for the wrong operation",
                None,
            )
        }
    }
}

fn handle_worker_failed(failed: WorkerFailed) -> Vec<Command> {
    finish_failed(failed.worker, "worker-failed", &failed.message)
}

fn handle_worker_cancelled(worker: u64) -> Vec<Command> {
    finish_failed(worker, "worker-cancelled", "file worker was cancelled")
}

fn finish_failed(worker: u64, code: &str, message: &str) -> Vec<Command> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    state.completed_snapshots.remove(&worker);
    let Some(job) = state.jobs.remove(&worker) else {
        state.suppressed_jobs.remove(&worker);
        return Vec::new();
    };
    let suppressed = state.suppressed_jobs.remove(&worker);
    if let Job::Save {
        actor,
        path,
        transfer,
        ..
    } = &job
    {
        state.release_save_job(actor, path, *transfer);
    }
    if suppressed {
        return Vec::new();
    }
    let (actor, request, transfer) = job_response(&job);
    drop(state);
    error_reply(&actor, request, transfer, code, message, None)
}

fn job_actor(job: &Job) -> &str {
    match job {
        Job::Open { actor, .. } | Job::Save { actor, .. } => actor,
    }
}

fn job_response(job: &Job) -> (String, u64, Option<u64>) {
    match job {
        Job::Open { actor, request, .. } => (actor.clone(), *request, None),
        Job::Save {
            actor,
            request,
            transfer,
            ..
        } => (actor.clone(), *request, Some(*transfer)),
    }
}

fn file_chunk_response(
    open: bool,
    request: u64,
    path: String,
    snapshot: &Snapshot,
    offset: u64,
) -> Result<Response, ProtocolError> {
    let offset = usize::try_from(offset)
        .map_err(|_overflow| ProtocolError::new("invalid-offset", "read offset overflowed"))?;
    if offset > snapshot.bytes.len() {
        return Err(ProtocolError::new(
            "invalid-offset",
            "read offset is beyond the end of the file",
        ));
    }
    let mut end = offset
        .saturating_add(MAX_CHUNK_BYTES)
        .min(snapshot.bytes.len());
    loop {
        let data_hex = encode_hex(
            snapshot
                .bytes
                .get(offset..end)
                .ok_or_else(|| ProtocolError::new("invalid-offset", "read offset is invalid"))?,
        );
        let response = if open {
            Response::Open {
                request,
                path: path.clone(),
                revision: snapshot.revision.clone(),
                size: snapshot.bytes.len() as u64,
                offset: offset as u64,
                data_hex,
                eof: end == snapshot.bytes.len(),
            }
        } else {
            Response::Read {
                request,
                path: path.clone(),
                revision: snapshot.revision.clone(),
                size: snapshot.bytes.len() as u64,
                offset: offset as u64,
                data_hex,
                eof: end == snapshot.bytes.len(),
            }
        };
        let encoded = serde_json::to_vec(&response)
            .map_err(|_error| ProtocolError::new("internal", "could not encode file chunk"))?;
        if encoded.len() <= MAX_WIRE_BYTES {
            return Ok(response);
        }
        if end == offset {
            return Err(ProtocolError::new(
                "message-too-large",
                "file path leaves no room for a bounded chunk response",
            ));
        }
        let excess = encoded.len().saturating_sub(MAX_WIRE_BYTES);
        let raw_reduction = excess.saturating_add(1) / 2;
        end = end.saturating_sub(raw_reduction.max(1)).max(offset);
    }
}

const fn response_eof(response: &Response) -> bool {
    match response {
        Response::Open { eof, .. } | Response::Read { eof, .. } => *eof,
        _ => false,
    }
}

fn response_next_offset(response: &Response) -> usize {
    match response {
        Response::Open {
            offset, data_hex, ..
        }
        | Response::Read {
            offset, data_hex, ..
        } => usize::try_from(*offset)
            .unwrap_or(usize::MAX)
            .saturating_add(data_hex.len() / 2),
        _ => 0,
    }
}

fn normalize_path(path: &str) -> Result<String, ProtocolError> {
    if path.is_empty() {
        return Err(ProtocolError::new("invalid-path", "path must not be empty"));
    }
    if path.len() > MAX_PATH_BYTES || path.bytes().any(|byte| byte == 0 || byte < b' ') {
        return Err(ProtocolError::new(
            "invalid-path",
            "path is too long or contains a control byte",
        ));
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(ProtocolError::new(
            "invalid-path",
            "path must be workspace-relative",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => normalized.push(component),
            Component::CurDir => {
                return Err(ProtocolError::new(
                    "invalid-path",
                    "path must not contain current-directory components",
                ));
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ProtocolError::new(
                    "invalid-path",
                    "path must not escape the workspace",
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(ProtocolError::new(
            "invalid-path",
            "path must name a regular file",
        ));
    }
    normalized
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| ProtocolError::new("invalid-path", "path must be valid UTF-8"))
}

fn normalize_revision(revision: &str) -> Result<String, ProtocolError> {
    if revision.len() != 64 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ProtocolError::new(
            "invalid-revision",
            "base revision must be a 64-digit SHA-256 value",
        ));
    }
    Ok(revision.to_ascii_lowercase())
}

fn presentation_actor(sender: &str) -> Option<String> {
    let actor = sender.strip_prefix("presentation:")?;
    valid_presentation_actor(actor).then(|| actor.to_string())
}

fn valid_presentation_actor(actor: &str) -> bool {
    !(actor.is_empty()
        || actor.len() > MAX_ACTOR_BYTES
        || !actor
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
}

fn revision(bytes: &[u8]) -> String {
    encode_hex(&Sha256::digest(bytes))
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>, ProtocolError> {
    if !encoded.len().is_multiple_of(2) {
        return Err(ProtocolError::new(
            "invalid-chunk",
            "hex chunk must contain complete byte pairs",
        ));
    }
    let mut decoded = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let high = decode_nibble(pair[0]).ok_or_else(|| {
            ProtocolError::new("invalid-chunk", "hex chunk contains a non-hex digit")
        })?;
        let low = decode_nibble(pair[1]).ok_or_else(|| {
            ProtocolError::new("invalid-chunk", "hex chunk contains a non-hex digit")
        })?;
        decoded.push((high << 4) | low);
    }
    Ok(decoded)
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn reply(actor: &str, response: Response) -> Vec<Command> {
    let request = response.request();
    let transfer = response.transfer();
    let body = match serde_json::to_vec(&response) {
        Ok(body) if body.len() <= MAX_WIRE_BYTES => body,
        Ok(_body) => match serde_json::to_vec(&error_response(
            request,
            transfer,
            "message-too-large",
            "response exceeds the cross-placement message limit",
            None,
        )) {
            Ok(body) => body,
            Err(_error) => return Vec::new(),
        },
        Err(_error) => return Vec::new(),
    };
    vec![Command::SendPresentationMessage(PresentationActorMessage {
        actor: actor.to_string(),
        body,
    })]
}

fn control_reply(actor: &str, response: ControlResponse) -> Vec<Command> {
    let body = match serde_json::to_vec(&response) {
        Ok(body) if body.len() <= MAX_WIRE_BYTES => body,
        _ => return Vec::new(),
    };
    vec![Command::SendPresentationMessage(PresentationActorMessage {
        actor: actor.to_string(),
        body,
    })]
}

fn protocol_error(
    actor: &str,
    request: u64,
    transfer: Option<u64>,
    error: ProtocolError,
) -> Vec<Command> {
    error_reply(actor, request, transfer, error.code, error.message, None)
}

fn internal_error(actor: &str, request: u64, transfer: Option<u64>) -> Vec<Command> {
    error_reply(
        actor,
        request,
        transfer,
        "internal",
        "file actor state is unavailable",
        None,
    )
}

fn error_reply(
    actor: &str,
    request: u64,
    transfer: Option<u64>,
    code: &str,
    message: &str,
    current_revision: Option<String>,
) -> Vec<Command> {
    reply(
        actor,
        error_response(request, transfer, code, message, current_revision),
    )
}

fn error_response(
    request: u64,
    transfer: Option<u64>,
    code: &str,
    message: &str,
    current_revision: Option<String>,
) -> Response {
    Response::Error {
        request,
        transfer,
        code: bounded_text(code, 48),
        message: bounded_text(message, MAX_ERROR_BYTES),
        current_revision,
    }
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.get(..end).unwrap_or_default().to_string()
}

export!(NeovimFiles);

#[cfg(test)]
mod tests {
    use super::*;

    static STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset_state() {
        *STATE.lock().expect("file state") = State::new();
    }

    fn authorize(actor: &str, session_id: u64) {
        STATE.lock().expect("file state").client = Some(ClientBinding {
            session_id,
            actor: actor.to_string(),
        });
    }

    fn mailbox(sender: &str, body: serde_json::Value) -> Vec<Command> {
        handle_mailbox(MailboxMessage {
            sender: sender.to_string(),
            body: serde_json::to_vec(&body).expect("mailbox JSON"),
        })
    }

    fn spawned_worker(commands: Vec<Command>) -> u64 {
        let [Command::SpawnWorker(request)] = commands.as_slice() else {
            panic!("expected one worker spawn");
        };
        request.worker
    }

    fn reply_value(commands: Vec<Command>) -> serde_json::Value {
        let [Command::SendPresentationMessage(message)] = commands.as_slice() else {
            panic!("expected one presentation reply");
        };
        serde_json::from_slice(&message.body).expect("reply JSON")
    }

    fn finish_open(worker: u64, bytes: Vec<u8>) -> serde_json::Value {
        let revision = revision(&bytes);
        let size = bytes.len() as u64;
        STATE
            .lock()
            .expect("file state")
            .completed_snapshots
            .insert(
                worker,
                Snapshot {
                    revision: revision.clone(),
                    bytes,
                    next_offset: 0,
                },
            );
        let output =
            serde_json::to_vec(&WorkerOutput::Opened { revision, size }).expect("worker output");
        reply_value(handle_worker_finished(WorkerFinished { worker, output }))
    }

    #[test]
    fn protocol_decodes_every_request_shape() {
        let cases = [
            br#"{"kind":"hello","request":1}"#.as_slice(),
            br#"{"kind":"open","request":2,"path":"src/lib.rs"}"#,
            br#"{"kind":"read","request":3,"path":"src/lib.rs","offset":7168}"#,
            br#"{"kind":"open-abort","request":3}"#,
            br#"{"kind":"save-begin","request":4,"transfer":9,"path":"src/lib.rs","base_revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":2}"#,
            br#"{"kind":"save-chunk","request":5,"transfer":9,"offset":0,"data_hex":"6f6b"}"#,
            br#"{"kind":"save-commit","request":6,"transfer":9}"#,
            br#"{"kind":"save-abort","request":6,"transfer":9}"#,
        ];
        for encoded in cases {
            assert!(
                serde_json::from_slice::<Request>(encoded).is_ok(),
                "{}",
                String::from_utf8_lossy(encoded)
            );
        }
    }

    #[test]
    fn paths_are_workspace_relative_and_lexically_bounded() {
        assert_eq!(
            normalize_path("src/editor bridge.lua"),
            Ok("src/editor bridge.lua".to_string())
        );
        for invalid in [
            "",
            "/etc/passwd",
            "../secret",
            "src/../../secret",
            "./src/lib.rs",
        ] {
            assert_eq!(
                normalize_path(invalid).map_err(|error| error.code),
                Err("invalid-path"),
                "{invalid}"
            );
        }
    }

    #[test]
    fn chunks_are_hex_bounded_and_save_assembly_is_contiguous() {
        let mut save = SaveTransfer {
            request: 1,
            path: "src/lib.rs".to_string(),
            base_revision: revision(b"old"),
            declared_size: 5,
            bytes: Vec::new(),
            committing: false,
        };
        assert_eq!(save.append(0, "68656c"), Ok(3));
        assert_eq!(save.append(3, "6c6f"), Ok(5));
        assert_eq!(save.bytes, b"hello");
        assert_eq!(
            save.append(4, "").map_err(|error| error.code),
            Err("invalid-offset")
        );
        assert_eq!(
            save.append(5, "00").map_err(|error| error.code),
            Err("file-too-large")
        );
        assert_eq!(
            decode_hex("xyz").map_err(|error| error.code),
            Err("invalid-chunk")
        );
        assert_eq!(
            SaveTransfer {
                request: 1,
                path: "x".to_string(),
                base_revision: revision(b""),
                declared_size: MAX_CHUNK_BYTES + 1,
                bytes: Vec::new(),
                committing: false,
            }
            .append(0, &"00".repeat(MAX_CHUNK_BYTES + 1))
            .map_err(|error| error.code),
            Err("chunk-too-large")
        );
    }

    #[test]
    fn file_chunks_fit_the_shared_cross_placement_lane() {
        let snapshot = Snapshot {
            revision: revision(&vec![b'x'; MAX_FILE_BYTES]),
            bytes: vec![b'x'; MAX_FILE_BYTES],
            next_offset: 0,
        };
        let response = file_chunk_response(true, 7, "a".repeat(MAX_PATH_BYTES), &snapshot, 0)
            .expect("chunk response");
        let encoded = serde_json::to_vec(&response).expect("encode chunk response");
        assert!(encoded.len() <= MAX_WIRE_BYTES, "{}", encoded.len());
        let Response::Open { data_hex, eof, .. } = response else {
            panic!("expected open response");
        };
        assert!(!data_hex.is_empty());
        assert!(data_hex.len() <= MAX_CHUNK_BYTES * 2);
        assert!(!eof);
    }

    #[test]
    fn revisions_are_sha256_and_senders_must_be_presentation_authenticated() {
        assert_eq!(
            revision(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            presentation_actor("presentation:neovim-ui-b2-s7"),
            Some("neovim-ui-b2-s7".to_string())
        );
        assert_eq!(presentation_actor("session:neovim-ui"), None);
        assert_eq!(presentation_actor("presentation:../../host"), None);
    }

    #[test]
    fn only_session_init_can_bind_the_exact_presentation_client() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state();

        let bind = serde_json::json!({
            "kind": "bind-client",
            "session_id": 7,
            "actor": "neovim-ui-b2-s7",
        });
        assert!(mailbox("terminal-session", bind.clone()).is_empty());

        let denied = reply_value(mailbox(
            "presentation:unrelated-s7",
            serde_json::json!({"kind": "hello", "request": 9}),
        ));
        assert_eq!(denied["code"], "unauthorized");
        assert!(STATE.lock().expect("file state").client.is_none());

        let authorized = reply_value(mailbox("init", bind));
        assert_eq!(authorized["kind"], "editor-client-authorized");
        assert_eq!(authorized["session_id"], 7);
        assert_eq!(authorized["actor"], "neovim-ui-b2-s7");

        let hello = reply_value(mailbox(
            "presentation:neovim-ui-b2-s7",
            serde_json::json!({"kind": "hello", "request": 10}),
        ));
        assert_eq!(hello["kind"], "hello");

        let unrelated = reply_value(mailbox(
            "presentation:neovim-ui-b3-s7",
            serde_json::json!({"kind": "open", "request": 11, "path": "secret"}),
        ));
        assert_eq!(unrelated["code"], "unauthorized");
        assert!(STATE.lock().expect("file state").jobs.is_empty());

        assert!(
            mailbox(
                "init",
                serde_json::json!({
                    "kind": "revoke-client",
                    "session_id": 7,
                    "actor": "neovim-ui-b3-s7",
                }),
            )
            .is_empty()
        );
        assert!(
            STATE
                .lock()
                .expect("file state")
                .client_is_authorized("neovim-ui-b2-s7")
        );

        assert!(
            mailbox(
                "init",
                serde_json::json!({
                    "kind": "revoke-client",
                    "session_id": 7,
                    "actor": "neovim-ui-b2-s7",
                }),
            )
            .is_empty()
        );
        assert!(STATE.lock().expect("file state").client.is_none());
    }

    #[test]
    fn concurrent_same_path_opens_keep_independent_immutable_snapshots() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state();

        let first_worker = spawned_worker(start_open("editor", 10, "src/lib.rs"));
        let duplicate = reply_value(start_open("editor", 10, "src/other.rs"));
        assert_eq!(duplicate["kind"], "error");
        assert_eq!(duplicate["code"], "duplicate-request");

        let second_worker = spawned_worker(start_open("editor", 11, "src/lib.rs"));
        let first_bytes = vec![b'a'; MAX_CHUNK_BYTES + 1];
        let second_bytes = vec![b'b'; MAX_CHUNK_BYTES + 1];

        let second_open = finish_open(second_worker, second_bytes.clone());
        let first_open = finish_open(first_worker, first_bytes.clone());
        assert_eq!(first_open["revision"], revision(&first_bytes));
        assert_eq!(second_open["revision"], revision(&second_bytes));
        assert_eq!(STATE.lock().expect("file state").snapshots.len(), 2);

        let first_offset = first_open["data_hex"].as_str().expect("first chunk").len() / 2;
        let second_offset = second_open["data_hex"]
            .as_str()
            .expect("second chunk")
            .len()
            / 2;
        let first_eof = reply_value(read_snapshot(
            "editor",
            10,
            "src/lib.rs",
            first_offset as u64,
        ));
        let second_eof = reply_value(read_snapshot(
            "editor",
            11,
            "src/lib.rs",
            second_offset as u64,
        ));
        assert_eq!(first_eof["data_hex"], "61");
        assert_eq!(second_eof["data_hex"], "62");
        assert_eq!(first_eof["eof"], true);
        assert_eq!(second_eof["eof"], true);
        assert!(STATE.lock().expect("file state").snapshots.is_empty());
    }

    #[test]
    fn inline_eof_releases_open_slots_for_sequential_files() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state();

        for request in 1..=(MAX_OPEN_FILES as u64 + 3) {
            let path = format!("file-{request}.txt");
            let worker = spawned_worker(start_open("editor", request, &path));
            let response = finish_open(worker, format!("file {request}").into_bytes());
            assert_eq!(response["kind"], "open");
            assert_eq!(response["eof"], true);
            let state = STATE.lock().expect("file state");
            assert!(state.jobs.is_empty());
            assert!(state.snapshots.is_empty());
        }
    }

    #[test]
    fn invalid_read_sequences_release_their_snapshot_slots() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state();

        let cases = [
            ("wrong-path", "src/other.rs", None),
            ("malformed-path", "../escape", None),
            ("out-of-order", "src/lib.rs", Some(1_u64)),
        ];
        for (index, (name, read_path, offset_adjustment)) in cases.into_iter().enumerate() {
            let request = index as u64 + 1;
            let worker = spawned_worker(start_open("editor", request, "src/lib.rs"));
            let response = finish_open(worker, vec![b'x'; MAX_CHUNK_BYTES + 5]);
            let next = (response["data_hex"].as_str().expect("initial chunk").len() / 2) as u64;
            assert_eq!(
                STATE.lock().expect("file state").snapshots.len(),
                1,
                "{name}"
            );
            let reply = reply_value(read_snapshot(
                "editor",
                request,
                read_path,
                next.saturating_add(offset_adjustment.unwrap_or_default()),
            ));
            assert_eq!(reply["kind"], "error", "{name}");
            assert!(
                STATE.lock().expect("file state").snapshots.is_empty(),
                "{name}"
            );
        }

        authorize("editor", 3);
        let worker = spawned_worker(start_open("editor", 20, "src/lib.rs"));
        let _open = finish_open(worker, vec![b'x'; MAX_CHUNK_BYTES + 5]);
        let malformed = reply_value(mailbox(
            "presentation:editor",
            serde_json::json!({
                "kind": "read",
                "request": 20,
                "path": "src/lib.rs",
                "offset": 1,
                "unexpected": true,
            }),
        ));
        assert_eq!(malformed["code"], "invalid-request");
        assert!(STATE.lock().expect("file state").snapshots.is_empty());
    }

    #[test]
    fn explicit_client_aborts_release_open_and_save_slots() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state();

        let worker = spawned_worker(start_open("editor", 20, "src/lib.rs"));
        let _open = finish_open(worker, vec![b'x'; MAX_CHUNK_BYTES + 5]);
        assert_eq!(STATE.lock().expect("file state").snapshots.len(), 1);
        assert!(abort_open_request("editor", 20).is_empty());
        assert!(STATE.lock().expect("file state").snapshots.is_empty());

        let pending_open = spawned_worker(start_open("editor", 21, "src/pending.rs"));
        assert!(matches!(
            abort_open_request("editor", 21).as_slice(),
            [Command::CancelWorker(worker)] if *worker == pending_open
        ));
        assert!(handle_worker_cancelled(pending_open).is_empty());
        assert!(STATE.lock().expect("file state").jobs.is_empty());

        let base = revision(b"old");
        let _ready = reply_value(start_save("editor", 22, 9, "src/lib.rs", &base, 2));
        assert_eq!(STATE.lock().expect("file state").transfers.len(), 1);
        assert!(abort_save_request("editor", 22, 9).is_empty());
        assert!(STATE.lock().expect("file state").transfers.is_empty());

        let _ready = reply_value(start_save("editor", 23, 10, "src/lib.rs", &base, 2));
        let _accepted = reply_value(append_save("editor", 23, 10, 0, "6f6b"));
        let pending_save = spawned_worker(commit_save("editor", 23, 10));
        assert!(matches!(
            abort_save_request("editor", 23, 10).as_slice(),
            [Command::CancelWorker(worker)] if *worker == pending_save
        ));
        assert!(handle_worker_cancelled(pending_save).is_empty());
        let state = STATE.lock().expect("file state");
        assert!(state.jobs.is_empty());
        assert!(state.transfers.is_empty());
        assert!(state.path_locks.is_empty());
    }

    #[test]
    fn malformed_or_incomplete_saves_release_transfer_slots() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state();
        let base = revision(b"old");

        let _ready = reply_value(start_save("editor", 1, 1, "file", &base, 2));
        let wrong_offset = reply_value(append_save("editor", 1, 1, 1, "61"));
        assert_eq!(wrong_offset["code"], "invalid-offset");
        assert!(STATE.lock().expect("file state").transfers.is_empty());

        let _ready = reply_value(start_save("editor", 2, 2, "file", &base, 2));
        let invalid_hex = reply_value(append_save("editor", 2, 2, 0, "zz"));
        assert_eq!(invalid_hex["code"], "invalid-chunk");
        assert!(STATE.lock().expect("file state").transfers.is_empty());

        let _ready = reply_value(start_save("editor", 3, 3, "file", &base, 2));
        let _accepted = reply_value(append_save("editor", 3, 3, 0, "61"));
        let incomplete = reply_value(commit_save("editor", 3, 3));
        assert_eq!(incomplete["code"], "incomplete-save");
        assert!(STATE.lock().expect("file state").transfers.is_empty());

        let _ready = reply_value(start_save("editor", 4, 4, "file", &base, 1));
        let wrong_request = reply_value(append_save("editor", 99, 4, 0, "61"));
        assert_eq!(wrong_request["code"], "invalid-request");
        assert!(STATE.lock().expect("file state").transfers.is_empty());

        authorize("editor", 3);
        let _ready = reply_value(start_save("editor", 5, 5, "file", &base, 1));
        let malformed = reply_value(mailbox(
            "presentation:editor",
            serde_json::json!({
                "kind": "save-chunk",
                "request": 5,
                "transfer": 5,
                "offset": 0,
                "data_hex": "61",
                "unexpected": true,
            }),
        ));
        assert_eq!(malformed["code"], "invalid-request");
        assert!(STATE.lock().expect("file state").transfers.is_empty());

        {
            let mut state = STATE.lock().expect("file state");
            for index in 0..MAX_ACTIVE_JOBS {
                state.jobs.insert(
                    10_000 + index as u64,
                    Job::Open {
                        actor: "other".to_string(),
                        path: format!("busy-{index}"),
                        request: 10_000 + index as u64,
                    },
                );
            }
        }
        let _ready = reply_value(start_save("editor", 7, 7, "file", &base, 1));
        let _accepted = reply_value(append_save("editor", 7, 7, 0, "61"));
        let no_worker = reply_value(commit_save("editor", 7, 7));
        assert_eq!(no_worker["code"], "limit-reached");
        assert!(
            STATE.lock().expect("file state").transfers.is_empty(),
            "pre-worker commit failure retained a save transfer"
        );
        STATE.lock().expect("file state").jobs.clear();
    }

    fn complete_save(actor: &str, request: u64, transfer: u64, path: &str, base: &str) {
        let ready = reply_value(start_save(actor, request, transfer, path, base, 3));
        assert_eq!(ready["kind"], "save-ready");
        let accepted = reply_value(append_save(actor, request, transfer, 0, "6e6577"));
        assert_eq!(accepted["kind"], "save-chunk-accepted");
    }

    #[test]
    fn commits_are_serialized_per_path_and_locks_end_with_every_worker_outcome() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state();
        let base = revision(b"old");

        complete_save("editor", 10, 1, "same.txt", &base);
        complete_save("editor", 11, 2, "same.txt", &base);
        let first_worker = spawned_worker(commit_save("editor", 10, 1));
        let busy = reply_value(commit_save("editor", 11, 2));
        assert_eq!(busy["code"], "path-busy");
        {
            let state = STATE.lock().expect("file state");
            assert_eq!(state.path_locks.len(), 1);
            assert!(!state.transfers.contains_key(&TransferKey {
                actor: "editor".to_string(),
                transfer: 2,
            }));
        }

        let saved = serde_json::to_vec(&WorkerOutput::Saved {
            revision: revision(b"new"),
            size: 3,
        })
        .expect("save output");
        let complete = reply_value(handle_worker_finished(WorkerFinished {
            worker: first_worker,
            output: saved,
        }));
        assert_eq!(complete["kind"], "save-complete");
        assert!(STATE.lock().expect("file state").path_locks.is_empty());

        complete_save("editor", 11, 2, "same.txt", &base);
        let second_worker = spawned_worker(commit_save("editor", 11, 2));
        let conflict = serde_json::to_vec(&WorkerOutput::Conflict {
            current_revision: revision(b"new"),
        })
        .expect("conflict output");
        let conflict = reply_value(handle_worker_finished(WorkerFinished {
            worker: second_worker,
            output: conflict,
        }));
        assert_eq!(conflict["code"], "conflict");
        assert!(STATE.lock().expect("file state").path_locks.is_empty());

        complete_save("failed", 12, 3, "same.txt", &base);
        let failed_worker = spawned_worker(commit_save("failed", 12, 3));
        let failed = reply_value(handle_worker_failed(WorkerFailed {
            worker: failed_worker,
            message: "boom".to_string(),
        }));
        assert_eq!(failed["code"], "worker-failed");
        assert!(STATE.lock().expect("file state").path_locks.is_empty());

        complete_save("cancelled", 13, 4, "same.txt", &base);
        let cancelled_worker = spawned_worker(commit_save("cancelled", 13, 4));
        let cancelled = reply_value(handle_worker_cancelled(cancelled_worker));
        assert_eq!(cancelled["code"], "worker-cancelled");
        let state = STATE.lock().expect("file state");
        assert!(state.path_locks.is_empty());
        assert!(state.jobs.is_empty());
        assert!(state.transfers.is_empty());
    }

    #[test]
    fn revoke_cancels_jobs_but_holds_commit_lock_until_terminal_cancellation() {
        let _serial = STATE_TEST_LOCK.lock().expect("state test lock");
        reset_state();
        authorize("editor", 9);
        let base = revision(b"old");
        complete_save("editor", 14, 5, "same.txt", &base);
        let worker = spawned_worker(commit_save("editor", 14, 5));

        let commands = mailbox(
            "init",
            serde_json::json!({
                "kind": "revoke-client",
                "session_id": 9,
                "actor": "editor",
            }),
        );
        assert!(matches!(
            commands.as_slice(),
            [Command::CancelWorker(cancelled)] if *cancelled == worker
        ));
        {
            let state = STATE.lock().expect("file state");
            assert!(state.client.is_none());
            assert_eq!(state.path_locks.len(), 1);
            assert!(state.suppressed_jobs.contains(&worker));
        }

        assert!(handle_worker_cancelled(worker).is_empty());
        let state = STATE.lock().expect("file state");
        assert!(state.path_locks.is_empty());
        assert!(state.jobs.is_empty());
        assert!(state.suppressed_jobs.is_empty());
    }
}
