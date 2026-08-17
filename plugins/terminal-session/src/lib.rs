use std::{collections::VecDeque, sync::Mutex};

use brood_graphics::theme::ashen;
use brood_plugin_sdk::{
    bindings,
    brood::plugin::host,
    exports::brood::plugin::actor::{
        Command, Event, Guest, LifecycleEvent, LocalActorMessage, NotificationEvent,
        PresentationActorMessage, WorkerEvent, WorkerRequest,
    },
    runtime::{
        sleep as wait_duration, wait_child, wait_notification, wait_timer,
        write_all as write_worker_bytes,
    },
};
use serde_json::{Map, Value};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

const WORKER: u64 = 1;
const EDITOR_PROBE_WORKER: u64 = 2;
const FIRST_EDITOR_CONTROL_WORKER: u64 = 3;
const SNAPSHOT_TOKEN: u64 = 1;
const STREAM_TOKEN: u64 = 2;
const EDITOR_SESSION_ID: &str = "neovim-session";
const CLIENT_INIT_ID: &str = "brood.client-init";
const INBOX_ID: &str = "inbox";
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;
const MAX_PENDING_INPUT: usize = 64 * 1024;
const MAX_PENDING_EDITOR_CONTROLS: usize = 32;
const MAX_EDITOR_PATH_BYTES: usize = 4 * 1024;
const MAX_EDITOR_OSC_JSON_BYTES: usize = 8 * 1024;
const MAX_EDITOR_OSC_QUEUE_BYTES: usize = 32 * 1024;
const MAX_EDITOR_OSC_ACTIONS: usize = 32;
const MAX_EDITOR_TARGET_BYTES: usize = 128;
const MAX_EDITOR_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_EDITOR_ACTION_FIELD_BYTES: usize = 8 * 1024;
const MAX_EDITOR_PROCESS_OUTPUT: usize = 16 * 1024;
const EDITOR_REMOTE_ATTEMPTS: usize = 40;
const EDITOR_REMOTE_RETRY_MS: u64 = 50;
const EDITOR_REMOTE_ATTEMPT_TIMEOUT_MS: u64 = 1_000;
const MAX_SCROLLBACK: usize = 2_000;
const MAX_SNAPSHOT_BODY: usize = 12 * 1024;
const BULK_OUTPUT_CHUNK_BYTES: usize = 4 * 1024;
const SNAPSHOT_MAGIC: &[u8; 4] = b"BTRM";
const SNAPSHOT_VERSION: u8 = 4;
const SNAPSHOT_FULL: u8 = 1;
const DEFAULT_FG: u32 = ashen::TEXT_RGB;
const DEFAULT_BG: u32 = ashen::BACKGROUND_RGB;
const SHELL_INTEGRATION: &str =
    r#"PS1='$(code=$?; printf "\033]777;brood-inbox;%s\007" "$code")$ '; export PS1; exec sh -i"#;

static STATE: Mutex<Option<SessionState>> = Mutex::new(None);

struct TerminalSession;

struct PendingInputBoundary {
    sequence: u64,
    remaining: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditorOpen {
    path: String,
    line: Option<u32>,
    column: Option<u32>,
}

struct SessionState {
    session_id: u64,
    editor_role: bool,
    editor_socket: Option<String>,
    subscriber: Option<String>,
    snapshot_notification: u64,
    snapshot_generation: u64,
    stream_notification: Option<u64>,
    stream_generation: u64,
    child: Option<u64>,
    terminal: Option<u64>,
    pending_input: VecDeque<u8>,
    pending_input_boundaries: VecDeque<PendingInputBoundary>,
    last_written_input_sequence: u64,
    snapshot_input_sequence: u64,
    pending_editor_controls: VecDeque<EditorOpen>,
    active_editor_control: Option<(u64, EditorOpen)>,
    editor_probe_pending: bool,
    editor_ready: bool,
    next_editor_control_worker: u64,
    interactive_flush_pending: bool,
    screen: Screen,
    viewport_offset: usize,
    revision: u64,
    chunks_read: u64,
    bytes_read: u64,
    resizes: u64,
    resize_output_batches: u8,
    snapshots_published: u64,
    published_rows: Vec<Vec<u8>>,
    force_full_snapshot: bool,
    interactive_flushes: u64,
    batch_waits: u64,
    exited: bool,
    exit_code: Option<i32>,
    error: Option<String>,
}

impl SessionState {
    fn new(
        session_id: u64,
        snapshot_notification: u64,
        editor_role: bool,
        editor_socket: Option<String>,
    ) -> Self {
        Self {
            session_id,
            editor_role,
            editor_socket,
            subscriber: None,
            snapshot_notification,
            snapshot_generation: 0,
            stream_notification: None,
            stream_generation: 0,
            child: None,
            terminal: None,
            pending_input: VecDeque::new(),
            pending_input_boundaries: VecDeque::new(),
            last_written_input_sequence: 0,
            snapshot_input_sequence: 0,
            pending_editor_controls: VecDeque::new(),
            active_editor_control: None,
            editor_probe_pending: false,
            editor_ready: false,
            next_editor_control_worker: FIRST_EDITOR_CONTROL_WORKER,
            interactive_flush_pending: false,
            screen: Screen::new(INITIAL_ROWS, INITIAL_COLS, editor_role),
            viewport_offset: 0,
            revision: 1,
            chunks_read: 0,
            bytes_read: 0,
            resizes: 0,
            resize_output_batches: 0,
            snapshots_published: 0,
            published_rows: Vec::new(),
            force_full_snapshot: true,
            interactive_flushes: 0,
            batch_waits: 0,
            exited: false,
            exit_code: None,
            error: None,
        }
    }

    fn mark_changed(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.snapshot_generation = self.snapshot_generation.saturating_add(1);
    }

    fn signal_changed(&mut self) {
        self.mark_changed();
        let _ = host::notification_signal(self.snapshot_notification, self.snapshot_generation);
    }

    fn queue_input(&mut self, sequence: u64, bytes: &[u8]) {
        let available = MAX_PENDING_INPUT.saturating_sub(self.pending_input.len());
        let accepted = bytes.len().min(available);
        self.pending_input
            .extend(bytes.iter().copied().take(accepted));
        if accepted > 0 {
            self.screen.record_submitted_input(&bytes[..accepted]);
            self.pending_input_boundaries
                .push_back(PendingInputBoundary {
                    sequence,
                    remaining: accepted,
                });
        }
        self.interactive_flush_pending |= accepted > 0;
        if available < bytes.len() {
            self.error = Some("terminal input queue reached its bounded capacity".to_string());
            self.mark_changed();
        }
    }

    fn flush_input(&mut self) {
        let Some(terminal) = self.terminal else {
            return;
        };
        loop {
            if self.pending_input.is_empty() {
                return;
            }
            let contiguous = self.pending_input.make_contiguous();
            match host::stream_write(terminal, contiguous) {
                Ok(written) if written > 0 => {
                    let written = written as usize;
                    self.pending_input.drain(..written);
                    self.record_written_input(written);
                }
                Ok(_) | Err(host::RuntimeIoError::WouldBlock) => {
                    self.watch_stream();
                    return;
                }
                Err(error) => {
                    self.error = Some(format!("terminal input failed: {error:?}"));
                    self.pending_input.clear();
                    self.pending_input_boundaries.clear();
                    self.mark_changed();
                    return;
                }
            }
        }
    }

    fn record_written_input(&mut self, mut written: usize) {
        while written > 0 {
            let Some(boundary) = self.pending_input_boundaries.front_mut() else {
                return;
            };
            let consumed = written.min(boundary.remaining);
            boundary.remaining = boundary.remaining.saturating_sub(consumed);
            written = written.saturating_sub(consumed);
            if boundary.remaining == 0 {
                self.last_written_input_sequence =
                    self.last_written_input_sequence.max(boundary.sequence);
                self.pending_input_boundaries.pop_front();
            }
        }
    }

    fn watch_stream(&mut self) {
        let Some(notification) = self.stream_notification else {
            return;
        };
        let _ = host::notification_watch_actor(notification, self.stream_generation, STREAM_TOKEN);
    }

    fn snapshot_commands(&mut self) -> Vec<Command> {
        let mut commands = self
            .screen
            .take_editor_actions()
            .into_iter()
            .flat_map(|payload| route_editor_action(self.session_id, &payload))
            .collect::<Vec<_>>();
        let command_completions = self.screen.take_command_completions();
        if let Some(actor) = self.subscriber.as_deref() {
            commands.extend(
                command_completions
                    .into_iter()
                    .map(|status| terminal_inbox_message(actor, status)),
            );
        }
        if let Some(actor) = self.subscriber.clone() {
            let chunks = encode_snapshot(self);
            self.snapshots_published = self.snapshots_published.saturating_add(chunks.len() as u64);
            commands.extend(chunks.into_iter().map(|body| {
                Command::SendPresentationMessage(PresentationActorMessage {
                    actor: actor.clone(),
                    body,
                })
            }));
        }
        commands
    }

    fn queue_editor_open(&mut self, request: EditorOpen) -> Vec<Command> {
        if !self.editor_role {
            return Vec::new();
        }
        let queued =
            self.pending_editor_controls.len() + usize::from(self.active_editor_control.is_some());
        if queued >= MAX_PENDING_EDITOR_CONTROLS {
            let failure = "editor control queue reached its bounded capacity".to_string();
            self.error = Some(failure.clone());
            self.mark_changed();
            let mut commands = self.snapshot_commands();
            commands.push(editor_open_failure_message(
                self.session_id,
                &request,
                &failure,
            ));
            return commands;
        }
        self.pending_editor_controls.push_back(request);
        self.start_next_editor_control().into_iter().collect()
    }

    fn start_editor_probe(&mut self) -> Option<Command> {
        if !self.editor_role
            || self.editor_ready
            || self.editor_probe_pending
            || self.child.is_none()
        {
            return None;
        }
        let socket = self.editor_socket.as_ref()?;
        self.editor_probe_pending = true;
        encode_editor_probe(socket).map(|payload| {
            Command::SpawnWorker(WorkerRequest {
                worker: EDITOR_PROBE_WORKER,
                payload,
            })
        })
    }

    fn finish_editor_probe(&mut self, failure: Option<String>) -> Vec<Command> {
        if !self.editor_probe_pending {
            return Vec::new();
        }
        self.editor_probe_pending = false;
        if let Some(failure) = failure {
            self.error = Some(failure.clone());
            self.mark_changed();
            let mut commands = self.snapshot_commands();
            commands.push(editor_lifecycle_message(
                "editor-failed",
                self.session_id,
                Some(&failure),
            ));
            return commands;
        }
        self.editor_ready = true;
        let mut commands = vec![editor_lifecycle_message(
            "editor-ready",
            self.session_id,
            None,
        )];
        commands.extend(self.start_next_editor_control());
        commands
    }

    fn start_next_editor_control(&mut self) -> Option<Command> {
        if !self.editor_ready || self.active_editor_control.is_some() {
            return None;
        }
        let socket = self.editor_socket.as_ref()?;
        let request = self.pending_editor_controls.pop_front()?;
        let worker = self.next_editor_control_worker;
        self.next_editor_control_worker = self
            .next_editor_control_worker
            .saturating_add(1)
            .max(FIRST_EDITOR_CONTROL_WORKER);
        let payload = encode_editor_control(socket, &request)?;
        self.active_editor_control = Some((worker, request));
        Some(Command::SpawnWorker(WorkerRequest { worker, payload }))
    }

    fn finish_editor_control(&mut self, worker: u64, failure: Option<String>) -> Vec<Command> {
        let Some((active_worker, request)) = self.active_editor_control.take() else {
            return Vec::new();
        };
        if active_worker != worker {
            self.active_editor_control = Some((active_worker, request));
            return Vec::new();
        }
        let mut commands = Vec::new();
        if let Some(failure) = failure {
            self.error = Some(failure.clone());
            self.mark_changed();
            commands.extend(self.snapshot_commands());
            commands.push(editor_open_failure_message(
                self.session_id,
                &request,
                &failure,
            ));
        }
        commands.extend(self.start_next_editor_control());
        commands
    }
}

fn terminal_inbox_message(target_actor: &str, status: i32) -> Command {
    let failed = status != 0;
    let body = serde_json::to_vec(&serde_json::json!({
        "kind": "inbox-push",
        "category": "terminal",
        "title": if failed { "Command failed" } else { "Command finished" },
        "detail": format!("exit status {status}"),
        "target_actor": target_actor,
        "status": if failed { "failed" } else { "completed" },
    }))
    .unwrap_or_else(|_| br#"{"kind":"inbox-push","title":"Command finished"}"#.to_vec());
    Command::SendPresentationMessage(PresentationActorMessage {
        actor: INBOX_ID.to_string(),
        body,
    })
}

impl Guest for TerminalSession {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(context)) => {
                let Some(session_id) = context.session_id else {
                    return vec![Command::LifecycleComplete(Err(
                        "terminal session actor has no surface session".to_string(),
                    ))];
                };
                let notification = match host::notification_create() {
                    Ok(notification) => notification,
                    Err(error) => {
                        return vec![Command::LifecycleComplete(Err(format!(
                            "create terminal snapshot notification: {error:?}"
                        )))];
                    }
                };
                let editor_role = is_editor_session_instance(&context.instance_id);
                let editor_socket = editor_role
                    .then(|| editor_socket_path(session_id, notification, &context.instance_id));
                if let Ok(mut state) = STATE.lock() {
                    *state = Some(SessionState::new(
                        session_id,
                        notification,
                        editor_role,
                        editor_socket,
                    ));
                }
                let _ = host::notification_watch_actor(notification, 0, SNAPSHOT_TOKEN);
                vec![
                    Command::SpawnWorker(WorkerRequest {
                        worker: WORKER,
                        payload: notification.to_le_bytes().to_vec(),
                    }),
                    Command::LifecycleComplete(Ok(())),
                ]
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![Command::LifecycleComplete(Ok(()))]
            }
            Event::Mailbox(message) => handle_mailbox(&message.sender, &message.body),
            Event::Notification(event) => handle_notification(event),
            Event::Worker(WorkerEvent::Finished(finished)) if finished.worker == WORKER => STATE
                .lock()
                .ok()
                .and_then(|state| state.as_ref().and_then(editor_worker_finished_message))
                .into_iter()
                .collect(),
            Event::Worker(WorkerEvent::Finished(finished))
                if finished.worker == EDITOR_PROBE_WORKER =>
            {
                let Ok(mut guard) = STATE.lock() else {
                    return Vec::new();
                };
                guard
                    .as_mut()
                    .map_or_else(Vec::new, |state| state.finish_editor_probe(None))
            }
            Event::Worker(WorkerEvent::Finished(finished)) if finished.worker != WORKER => {
                let Ok(mut guard) = STATE.lock() else {
                    return Vec::new();
                };
                guard.as_mut().map_or_else(Vec::new, |state| {
                    state.finish_editor_control(finished.worker, None)
                })
            }
            Event::Worker(WorkerEvent::Failed(failed)) if failed.worker == WORKER => {
                let Ok(mut guard) = STATE.lock() else {
                    return Vec::new();
                };
                let Some(state) = guard.as_mut() else {
                    return Vec::new();
                };
                state.error = Some(failed.message);
                state.exited = true;
                state.mark_changed();
                let editor_role = state.editor_role;
                let session_id = state.session_id;
                let mut commands = state.snapshot_commands();
                if editor_role {
                    commands.push(editor_lifecycle_message(
                        "editor-failed",
                        session_id,
                        state.error.as_deref(),
                    ));
                }
                commands
            }
            Event::Worker(WorkerEvent::Failed(failed)) if failed.worker == EDITOR_PROBE_WORKER => {
                let Ok(mut guard) = STATE.lock() else {
                    return Vec::new();
                };
                guard.as_mut().map_or_else(Vec::new, |state| {
                    state.finish_editor_probe(Some(failed.message))
                })
            }
            Event::Worker(WorkerEvent::Failed(failed)) => {
                let Ok(mut guard) = STATE.lock() else {
                    return Vec::new();
                };
                guard.as_mut().map_or_else(Vec::new, |state| {
                    state.finish_editor_control(failed.worker, Some(failed.message))
                })
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        if worker != WORKER {
            return run_editor_control(&payload).await;
        }
        let notification = payload
            .get(..8)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_le_bytes)
            .ok_or_else(|| "terminal worker notification payload is invalid".to_string())?;
        run_terminal_worker(notification).await
    }
}

fn handle_mailbox(sender: &str, body: &[u8]) -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    match body.first().copied() {
        Some(b'A') if sender.starts_with("presentation:") => {
            state.subscriber = sender.strip_prefix("presentation:").map(str::to_string);
            state.force_full_snapshot = true;
            state.snapshot_commands()
        }
        Some(b'I') if trusted_input_sender(sender) => {
            let revision = state.revision;
            state.viewport_offset = 0;
            let (sequence, bytes) = if let Some(encoded) = body.get(1..9)
                && let Ok(encoded) = <[u8; 8]>::try_from(encoded)
            {
                (
                    u64::from_le_bytes(encoded),
                    body.get(9..).unwrap_or_default(),
                )
            } else {
                (0, body.get(1..).unwrap_or_default())
            };
            state.queue_input(sequence, bytes);
            state.flush_input();
            if state.revision != revision {
                state.snapshot_commands()
            } else {
                Vec::new()
            }
        }
        Some(b'R') if sender.starts_with("presentation:") && body.len() == 5 => {
            let rows = u16::from_le_bytes([body[1], body[2]]);
            let cols = u16::from_le_bytes([body[3], body[4]]);
            if rows == 0 || cols == 0 {
                return Vec::new();
            }
            if state.screen.rows == rows && state.screen.cols == cols {
                return Vec::new();
            }
            state.screen.resize(rows, cols);
            state.force_full_snapshot = true;
            if let Some(child) = state.child
                && let Err(error) = host::pty_resize(child, rows, cols)
            {
                state.error = Some(format!("PTY resize failed: {error:?}"));
            }
            state.resizes = state.resizes.saturating_add(1);
            state.resize_output_batches = state.resize_output_batches.max(4);
            state.mark_changed();
            state.snapshot_commands()
        }
        Some(b'V') if sender.starts_with("presentation:") && body.len() == 5 => {
            let requested = u32::from_le_bytes([body[1], body[2], body[3], body[4]]) as usize;
            let viewport_offset = requested.min(state.screen.max_viewport_offset());
            if state.viewport_offset == viewport_offset {
                return Vec::new();
            }
            state.viewport_offset = viewport_offset;
            state.mark_changed();
            state.snapshot_commands()
        }
        Some(b'{') if state.editor_role => parse_editor_open(body)
            .map(|request| state.queue_editor_open(request))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn trusted_input_sender(sender: &str) -> bool {
    sender == "init" || sender.starts_with("presentation:")
}

fn editor_socket_path(session_id: u64, notification: u64, instance_id: &str) -> String {
    let instance_hash = instance_id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            hash.wrapping_mul(0x100_0000_01b3) ^ u64::from(*byte)
        });
    format!("/tmp/brood-nvim-s{session_id:x}-n{notification:x}-{instance_hash:x}.sock")
}

fn is_editor_session_instance(instance_id: &str) -> bool {
    instance_id == EDITOR_SESSION_ID
        || instance_id
            .strip_suffix(EDITOR_SESSION_ID)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn parse_editor_open(body: &[u8]) -> Option<EditorOpen> {
    if body.len() > MAX_EDITOR_MESSAGE_BYTES {
        return None;
    }
    let value = serde_json::from_slice::<Value>(body).ok()?;
    let object = value.as_object()?;
    if object.get("kind")?.as_str()? != "editor-open" {
        return None;
    }
    let path = object.get("path")?.as_str()?;
    if path.is_empty() || path.len() > MAX_EDITOR_PATH_BYTES || path.contains('\0') {
        return None;
    }
    let line = parse_editor_position(object.get("line"))?;
    let column = parse_editor_position(object.get("column"))?;
    Some(EditorOpen {
        path: path.to_string(),
        line: line.or_else(|| column.map(|_| 1)),
        column,
    })
}

fn parse_editor_position(value: Option<&Value>) -> Option<Option<u32>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(value) => value
            .as_u64()
            .filter(|value| *value > 0)
            .and_then(|value| u32::try_from(value).ok())
            .map(Some),
    }
}

fn route_editor_action(session_id: u64, payload: &[u8]) -> Vec<Command> {
    if payload.len() > MAX_EDITOR_OSC_JSON_BYTES {
        return Vec::new();
    }
    let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(payload) else {
        return Vec::new();
    };
    let Some(action) = object.get("action").and_then(Value::as_str) else {
        return Vec::new();
    };
    match action {
        "diff" | "terminal" | "agent" => {
            let mut forwarded = Map::new();
            forwarded.insert(
                "kind".to_string(),
                Value::String("editor-action".to_string()),
            );
            forwarded.insert("action".to_string(), Value::String(action.to_string()));
            forwarded.insert("session_id".to_string(), Value::from(session_id));
            for field in ["command", "prompt", "agent"] {
                if let Some(value) = object.get(field)
                    && bounded_json_value(value, MAX_EDITOR_ACTION_FIELD_BYTES)
                {
                    forwarded.insert(field.to_string(), value.clone());
                }
            }
            serde_json::to_vec(&Value::Object(forwarded))
                .ok()
                .filter(|body| body.len() <= MAX_EDITOR_MESSAGE_BYTES)
                .map(|body| {
                    Command::SendPresentationMessage(PresentationActorMessage {
                        actor: CLIENT_INIT_ID.to_string(),
                        body,
                    })
                })
                .into_iter()
                .collect()
        }
        "message" => route_editor_message(&object).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn editor_lifecycle_message(kind: &str, session_id: u64, message: Option<&str>) -> Command {
    let message = message
        .map(|message| message.chars().take(2_048).collect::<String>())
        .unwrap_or_default();
    let body = serde_json::to_vec(&serde_json::json!({
        "kind": kind,
        "session_id": session_id,
        "message": message,
    }))
    .unwrap_or_default();
    Command::SendLocalMessage(LocalActorMessage {
        actor: "init".to_string(),
        body,
    })
}

fn editor_worker_finished_message(state: &SessionState) -> Option<Command> {
    if !state.editor_role {
        return None;
    }
    if state.editor_ready && state.exit_code == Some(0) {
        return Some(editor_lifecycle_message(
            "editor-stopped",
            state.session_id,
            None,
        ));
    }
    let failure = state.error.clone().unwrap_or_else(|| {
        if state.editor_ready {
            format!(
                "Neovim exited unexpectedly with status {:?}",
                state.exit_code
            )
        } else {
            format!(
                "Neovim exited before its RPC server became ready with status {:?}",
                state.exit_code
            )
        }
    });
    Some(editor_lifecycle_message(
        "editor-failed",
        state.session_id,
        Some(&failure),
    ))
}

fn editor_open_failure_message(session_id: u64, request: &EditorOpen, failure: &str) -> Command {
    let failure = failure.chars().take(2_048).collect::<String>();
    let body = serde_json::to_vec(&serde_json::json!({
        "kind": "editor-open-failed",
        "session_id": session_id,
        "path": request.path,
        "message": failure,
    }))
    .unwrap_or_default();
    Command::SendPresentationMessage(PresentationActorMessage {
        actor: CLIENT_INIT_ID.to_string(),
        body,
    })
}

fn bounded_json_value(value: &Value, limit: usize) -> bool {
    !value.is_null() && serde_json::to_vec(value).is_ok_and(|encoded| encoded.len() <= limit)
}

fn route_editor_message(object: &Map<String, Value>) -> Option<Command> {
    let target = object.get("target")?.as_str()?;
    if target.is_empty() || target.len() > MAX_EDITOR_TARGET_BYTES {
        return None;
    }
    let body = match object.get("body")? {
        Value::String(body) => body.as_bytes().to_vec(),
        body => serde_json::to_vec(body).ok()?,
    };
    if body.len() > MAX_EDITOR_MESSAGE_BYTES {
        return None;
    }
    if let Some(actor) = target.strip_prefix("presentation:") {
        return valid_actor_target(actor)
            .then_some(actor)
            .filter(|actor| !matches!(*actor, CLIENT_INIT_ID | "session-switcher"))
            .map(|actor| {
                Command::SendPresentationMessage(PresentationActorMessage {
                    actor: actor.to_string(),
                    body,
                })
            });
    }
    let actor = target.strip_prefix("session:").unwrap_or(target);
    valid_actor_target(actor).then(|| {
        Command::SendLocalMessage(LocalActorMessage {
            actor: actor.to_string(),
            body,
        })
    })
}

fn valid_actor_target(actor: &str) -> bool {
    !actor.is_empty()
        && actor.len() <= MAX_EDITOR_TARGET_BYTES
        && actor
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn decode_hex(encoded: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.chunks_exact(2) {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        decoded.push((high << 4) | low);
    }
    Some(decoded)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_editor_control(socket: &str, request: &EditorOpen) -> Option<Vec<u8>> {
    let mut value = serde_json::json!({
        "kind": "editor-open-control",
        "socket": socket,
        "path": request.path,
    });
    let object = value.as_object_mut()?;
    if let Some(line) = request.line {
        object.insert("line".to_string(), Value::from(line));
    }
    if let Some(column) = request.column {
        object.insert("column".to_string(), Value::from(column));
    }
    serde_json::to_vec(&value)
        .ok()
        .filter(|payload| payload.len() <= MAX_EDITOR_MESSAGE_BYTES)
}

fn encode_editor_probe(socket: &str) -> Option<Vec<u8>> {
    serde_json::to_vec(&serde_json::json!({
        "kind": "editor-probe-control",
        "socket": socket,
    }))
    .ok()
    .filter(|payload| payload.len() <= MAX_EDITOR_MESSAGE_BYTES)
}

fn decode_editor_probe(payload: &[u8]) -> Option<String> {
    if payload.len() > MAX_EDITOR_MESSAGE_BYTES {
        return None;
    }
    let Value::Object(object) = serde_json::from_slice::<Value>(payload).ok()? else {
        return None;
    };
    if object.get("kind")?.as_str()? != "editor-probe-control" {
        return None;
    }
    let socket = object.get("socket")?.as_str()?;
    (!socket.is_empty() && socket.len() <= MAX_EDITOR_PATH_BYTES && !socket.contains('\0'))
        .then(|| socket.to_string())
}

fn decode_editor_control(payload: &[u8]) -> Option<(String, EditorOpen)> {
    if payload.len() > MAX_EDITOR_MESSAGE_BYTES {
        return None;
    }
    let Value::Object(object) = serde_json::from_slice::<Value>(payload).ok()? else {
        return None;
    };
    if object.get("kind")?.as_str()? != "editor-open-control" {
        return None;
    }
    let socket = object.get("socket")?.as_str()?;
    if socket.is_empty() || socket.len() > MAX_EDITOR_PATH_BYTES || socket.contains('\0') {
        return None;
    }
    let open = parse_editor_open(
        &serde_json::to_vec(&serde_json::json!({
            "kind": "editor-open",
            "path": object.get("path")?,
            "line": object.get("line"),
            "column": object.get("column"),
        }))
        .ok()?,
    )?;
    Some((socket.to_string(), open))
}

fn editor_remote_arguments(socket: &str, request: &EditorOpen) -> Vec<String> {
    let mut arguments = vec![
        "--server".to_string(),
        socket.to_string(),
        "--remote".to_string(),
    ];
    let path = if request.path.starts_with(['-', '+']) {
        format!("./{}", request.path)
    } else {
        request.path.clone()
    };
    arguments.push(path);
    arguments
}

fn editor_cursor_arguments(socket: &str, request: &EditorOpen) -> Option<Vec<String>> {
    let line = request.line?;
    let column = request.column.unwrap_or(1);
    Some(vec![
        "--server".to_string(),
        socket.to_string(),
        "--remote-expr".to_string(),
        format!("cursor({line},{column})"),
    ])
}

fn handle_notification(event: NotificationEvent) -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    match event.token {
        SNAPSHOT_TOKEN if event.notification == state.snapshot_notification => {
            state.snapshot_generation = state.snapshot_generation.max(event.generation);
            let mut commands = state.snapshot_commands();
            commands.extend(state.start_editor_probe());
            let _ = host::notification_watch_actor(
                state.snapshot_notification,
                state.snapshot_generation,
                SNAPSHOT_TOKEN,
            );
            commands
        }
        STREAM_TOKEN if Some(event.notification) == state.stream_notification => {
            state.stream_generation = state.stream_generation.max(event.generation);
            state.flush_input();
            Vec::new()
        }
        _ => Vec::new(),
    }
}

struct EditorProcessOutput {
    state: host::ChildState,
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_editor_control(payload: &[u8]) -> Result<Vec<u8>, String> {
    if let Some(socket) = decode_editor_probe(payload) {
        let arguments = vec![
            "--server".to_string(),
            socket,
            "--remote-expr".to_string(),
            "1".to_string(),
        ];
        run_editor_remote_with_retry(&arguments, "probe Neovim server").await?;
        return Ok(br#"{"kind":"editor-ready"}"#.to_vec());
    }
    let (socket, request) = decode_editor_control(payload)
        .ok_or_else(|| "editor control payload is invalid".to_string())?;
    let arguments = editor_remote_arguments(&socket, &request);
    let mut attempt =
        run_editor_remote_with_retry(&arguments, &format!("open {}", request.path)).await?;
    if let Some(arguments) = editor_cursor_arguments(&socket, &request) {
        attempt = attempt.max(
            run_editor_remote_with_retry(&arguments, &format!("position {}", request.path)).await?,
        );
    }
    Ok(format!(
        "{{\"kind\":\"editor-opened\",\"path\":{},\"attempt\":{attempt}}}",
        serde_json::to_string(&request.path).unwrap_or_else(|_| "\"<invalid>\"".to_string()),
    )
    .into_bytes())
}

async fn run_editor_remote_with_retry(
    arguments: &[String],
    operation: &str,
) -> Result<usize, String> {
    let mut last_failure = String::new();
    for attempt in 0..EDITOR_REMOTE_ATTEMPTS {
        match run_editor_remote_once(arguments).await {
            Ok(output) if output.state == host::ChildState::Exited && output.code == Some(0) => {
                return Ok(attempt + 1);
            }
            Ok(output) => {
                last_failure = editor_process_failure(&output);
            }
            Err(error) => last_failure = error,
        }
        if attempt + 1 < EDITOR_REMOTE_ATTEMPTS {
            wait_duration(EDITOR_REMOTE_RETRY_MS).await?;
        }
    }
    Err(format!(
        "{operation} failed after {} attempts: {}",
        EDITOR_REMOTE_ATTEMPTS, last_failure
    ))
}

async fn run_editor_remote_once(arguments: &[String]) -> Result<EditorProcessOutput, String> {
    let process = host::subprocess_spawn("nvim", arguments)
        .map_err(|error| format!("spawn nvim remote client: {error:?}"))?;
    let result = match host::stream_close_write(process.stdin) {
        Ok(()) => drain_editor_process(&process).await,
        Err(error) => Err(format!("close nvim remote stdin: {error:?}")),
    };
    let _ = host::stream_drop(process.stdin).await;
    let _ = host::stream_drop(process.stdout).await;
    let _ = host::stream_drop(process.stderr).await;
    let _ = host::child_drop(process.child).await;
    result
}

async fn drain_editor_process(process: &host::ProcessIo) -> Result<EditorProcessOutput, String> {
    let timer = host::timer_create(EDITOR_REMOTE_ATTEMPT_TIMEOUT_MS)
        .map_err(|error| format!("create Neovim remote deadline: {error:?}"))?;
    let result = drain_editor_process_until(process, timer).await;
    let _ = host::timer_drop(timer).await;
    result
}

async fn drain_editor_process_until(
    process: &host::ProcessIo,
    timer: u64,
) -> Result<EditorProcessOutput, String> {
    let stdout_notification = host::stream_notification(process.stdout)
        .map_err(|error| format!("nvim stdout notification: {error:?}"))?;
    let stderr_notification = host::stream_notification(process.stderr)
        .map_err(|error| format!("nvim stderr notification: {error:?}"))?;
    let child_notification = host::child_notification(process.child)
        .map_err(|error| format!("nvim child notification: {error:?}"))?;
    let timer_notification = host::timer_notification(timer)
        .map_err(|error| format!("Neovim remote deadline notification: {error:?}"))?;
    let mut stdout_generation = 0;
    let mut stderr_generation = 0;
    let mut child_generation = 0;
    let mut timer_generation = 0;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        stdout_eof |= drain_editor_stream(process.stdout, &mut stdout)?;
        stderr_eof |= drain_editor_stream(process.stderr, &mut stderr)?;
        let (state, code) = host::child_status(process.child)
            .map_err(|error| format!("nvim child status: {error:?}"))?;
        if state != host::ChildState::Running && stdout_eof && stderr_eof {
            return Ok(EditorProcessOutput {
                state,
                code,
                stdout,
                stderr,
            });
        }
        let stdout_pollable = host::notification_pollable(stdout_notification, stdout_generation)
            .map_err(|error| format!("nvim stdout pollable: {error:?}"))?;
        let stderr_pollable = host::notification_pollable(stderr_notification, stderr_generation)
            .map_err(|error| format!("nvim stderr pollable: {error:?}"))?;
        let child_pollable = host::notification_pollable(child_notification, child_generation)
            .map_err(|error| format!("nvim child pollable: {error:?}"))?;
        let timer_pollable = host::notification_pollable(timer_notification, timer_generation)
            .map_err(|error| format!("Neovim remote deadline pollable: {error:?}"))?;
        let ready = host::worker_wait_any(
            vec![
                stdout_pollable,
                stderr_pollable,
                child_pollable,
                timer_pollable,
            ],
            None,
            host::WaitMode::Fair,
        )
        .await;
        let _ = host::pollable_drop(stdout_pollable);
        let _ = host::pollable_drop(stderr_pollable);
        let _ = host::pollable_drop(child_pollable);
        let _ = host::pollable_drop(timer_pollable);
        match ready {
            Ok(ready) if ready.index == 0 => stdout_generation = ready.generation,
            Ok(ready) if ready.index == 1 => stderr_generation = ready.generation,
            Ok(ready) if ready.index == 2 => child_generation = ready.generation,
            Ok(ready) => {
                timer_generation = ready.generation;
                if host::timer_status(timer)
                    .map_err(|error| format!("Neovim remote deadline status: {error:?}"))?
                    == host::TimerState::Expired
                {
                    return Err(format!(
                        "Neovim remote control timed out after {EDITOR_REMOTE_ATTEMPT_TIMEOUT_MS}ms"
                    ));
                }
            }
            Err(host::WaitError::Cancelled) => {
                return Err("Neovim remote control was cancelled".to_string());
            }
            Err(error) => return Err(format!("wait for Neovim remote control: {error:?}")),
        }
    }
}

fn drain_editor_stream(stream: u64, output: &mut Vec<u8>) -> Result<bool, String> {
    loop {
        match host::stream_read(stream, 4096)
            .map_err(|error| format!("read Neovim remote stream: {error:?}"))?
        {
            host::StreamReadResult::Data(bytes) => {
                let remaining = MAX_EDITOR_PROCESS_OUTPUT.saturating_sub(output.len());
                output.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            }
            host::StreamReadResult::WouldBlock => return Ok(false),
            host::StreamReadResult::Eof => return Ok(true),
        }
    }
}

fn editor_process_failure(output: &EditorProcessOutput) -> String {
    let detail = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let detail = String::from_utf8_lossy(detail);
    let detail = detail.trim();
    if detail.is_empty() {
        format!("state {:?}, status {:?}", output.state, output.code)
    } else {
        format!(
            "state {:?}, status {:?}: {detail}",
            output.state, output.code
        )
    }
}

async fn run_terminal_worker(notification: u64) -> Result<Vec<u8>, String> {
    let (rows, cols, editor_socket) = {
        let guard = STATE
            .lock()
            .map_err(|_| "terminal state is poisoned".to_string())?;
        let state = guard
            .as_ref()
            .ok_or_else(|| "terminal state is unavailable".to_string())?;
        (
            state.screen.rows,
            state.screen.cols,
            state.editor_socket.clone(),
        )
    };
    let (command, arguments) = editor_socket.map_or_else(
        || ("sh", vec!["-c".to_string(), SHELL_INTEGRATION.to_string()]),
        |socket| ("brood-nvim", vec!["--listen".to_string(), socket]),
    );
    let pty = host::pty_spawn(
        command,
        &arguments,
        host::PtySpawnOptions {
            rows,
            cols,
            terminal_profile: host::TerminalProfile::Xterm256color,
        },
    )
    .map_err(|error| format!("spawn terminal role {command}: {error:?}"))?;
    let stream_notification = host::stream_notification(pty.terminal)
        .map_err(|error| format!("terminal stream notification: {error:?}"))?;
    {
        let mut guard = STATE
            .lock()
            .map_err(|_| "terminal state is poisoned".to_string())?;
        let state = guard
            .as_mut()
            .ok_or_else(|| "terminal state is unavailable".to_string())?;
        state.child = Some(pty.child);
        state.terminal = Some(pty.terminal);
        state.stream_notification = Some(stream_notification);
        state.flush_input();
        state.signal_changed();
    }

    let mut parser = Parser::new();
    let mut observed = 0;
    loop {
        match host::stream_read(pty.terminal, 16 * 1024)
            .map_err(|error| format!("read terminal stream: {error:?}"))?
        {
            host::StreamReadResult::Data(bytes) => {
                consume_terminal_bytes(&mut parser, &bytes)?;
                let interactive_flush = take_interactive_flush()?;
                if !interactive_flush
                    && (bytes.len() >= BULK_OUTPUT_CHUNK_BYTES
                        || !alternate_screen_active()?
                        || take_resize_output_batch()?)
                {
                    record_batch_wait()?;
                    let timer = host::timer_create(8)
                        .map_err(|error| format!("create snapshot timer: {error:?}"))?;
                    wait_timer(timer).await?;
                    let _ = host::timer_drop(timer).await;
                }
                let mut eof = false;
                loop {
                    match host::stream_read(pty.terminal, 16 * 1024)
                        .map_err(|error| format!("drain terminal stream: {error:?}"))?
                    {
                        host::StreamReadResult::Data(bytes) => {
                            consume_terminal_bytes(&mut parser, &bytes)?;
                        }
                        host::StreamReadResult::WouldBlock => break,
                        host::StreamReadResult::Eof => {
                            eof = true;
                            break;
                        }
                    }
                }
                let responses = publish_terminal_revision(notification)?;
                if !responses.is_empty() {
                    write_worker_bytes(pty.terminal, &responses).await?;
                }
                if eof {
                    break;
                }
            }
            host::StreamReadResult::WouldBlock => {
                observed = wait_notification(stream_notification, observed).await?;
            }
            host::StreamReadResult::Eof => break,
        }
    }

    let (exit_state, exit_code) = wait_child(pty.child).await?;
    let metrics = {
        let mut guard = STATE
            .lock()
            .map_err(|_| "terminal state is poisoned".to_string())?;
        let state = guard
            .as_mut()
            .ok_or_else(|| "terminal state is unavailable".to_string())?;
        state.exited = true;
        state.exit_code = exit_code;
        state.child = None;
        state.terminal = None;
        state.stream_notification = None;
        state.signal_changed();
        format!(
            "{{\"session\":{},\"chunks\":{},\"bytes\":{},\"revision\":{},\"interactive_flushes\":{},\"batch_waits\":{},\"exit\":\"{:?}\",\"code\":{}}}",
            state.session_id,
            state.chunks_read,
            state.bytes_read,
            state.revision,
            state.interactive_flushes,
            state.batch_waits,
            exit_state,
            exit_code.map_or_else(|| "null".to_string(), |code| code.to_string()),
        )
    };
    let _ = host::stream_drop(pty.terminal).await;
    let _ = host::child_drop(pty.child).await;
    Ok(metrics.into_bytes())
}

fn take_interactive_flush() -> Result<bool, String> {
    let mut guard = STATE
        .lock()
        .map_err(|_| "terminal state is poisoned".to_string())?;
    let state = guard
        .as_mut()
        .ok_or_else(|| "terminal state is unavailable".to_string())?;
    let pending = std::mem::take(&mut state.interactive_flush_pending);
    if pending {
        state.interactive_flushes = state.interactive_flushes.saturating_add(1);
    }
    Ok(pending)
}

fn record_batch_wait() -> Result<(), String> {
    let mut guard = STATE
        .lock()
        .map_err(|_| "terminal state is poisoned".to_string())?;
    let state = guard
        .as_mut()
        .ok_or_else(|| "terminal state is unavailable".to_string())?;
    state.batch_waits = state.batch_waits.saturating_add(1);
    Ok(())
}

fn take_resize_output_batch() -> Result<bool, String> {
    let mut guard = STATE
        .lock()
        .map_err(|_| "terminal state is poisoned".to_string())?;
    let state = guard
        .as_mut()
        .ok_or_else(|| "terminal state is unavailable".to_string())?;
    let pending = state.resize_output_batches > 0;
    state.resize_output_batches = state.resize_output_batches.saturating_sub(1);
    Ok(pending)
}

fn alternate_screen_active() -> Result<bool, String> {
    let guard = STATE
        .lock()
        .map_err(|_| "terminal state is poisoned".to_string())?;
    let state = guard
        .as_ref()
        .ok_or_else(|| "terminal state is unavailable".to_string())?;
    Ok(state.screen.alternate.is_some())
}

fn consume_terminal_bytes(parser: &mut Parser, bytes: &[u8]) -> Result<(), String> {
    let mut guard = STATE
        .lock()
        .map_err(|_| "terminal state is poisoned".to_string())?;
    let state = guard
        .as_mut()
        .ok_or_else(|| "terminal state is unavailable".to_string())?;
    let previous_scrollback = state.screen.scrollback.len();
    parser.advance(&mut state.screen, bytes);
    if state.viewport_offset > 0 {
        let added = state
            .screen
            .scrollback
            .len()
            .saturating_sub(previous_scrollback);
        state.viewport_offset = state
            .viewport_offset
            .saturating_add(added)
            .min(state.screen.max_viewport_offset());
    }
    state.chunks_read = state.chunks_read.saturating_add(1);
    state.bytes_read = state.bytes_read.saturating_add(bytes.len() as u64);
    state.snapshot_input_sequence = state
        .snapshot_input_sequence
        .max(state.last_written_input_sequence);
    Ok(())
}

fn publish_terminal_revision(notification: u64) -> Result<Vec<u8>, String> {
    let mut guard = STATE
        .lock()
        .map_err(|_| "terminal state is poisoned".to_string())?;
    let state = guard
        .as_mut()
        .ok_or_else(|| "terminal state is unavailable".to_string())?;
    state.revision = state.revision.saturating_add(1);
    state.snapshot_generation = state.snapshot_generation.saturating_add(1);
    let generation = state.snapshot_generation;
    let responses = std::mem::take(&mut state.screen.responses);
    let _ = host::notification_signal(notification, generation);
    Ok(responses)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Style {
    fg: u32,
    bg: u32,
    bold: bool,
    faint: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    inverse: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            bold: false,
            faint: false,
            italic: false,
            underline: false,
            strikethrough: false,
            inverse: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Cell {
    text: String,
    continuation: bool,
    style: Style,
}

impl Cell {
    fn blank(style: Style) -> Self {
        Self {
            text: " ".to_string(),
            continuation: false,
            style,
        }
    }
}

struct Screen {
    rows: u16,
    cols: u16,
    cells: Vec<Vec<Cell>>,
    scrollback: VecDeque<Vec<Cell>>,
    alternate: Option<(Vec<Vec<Cell>>, u16, u16)>,
    cursor_row: u16,
    cursor_col: u16,
    saved_cursor: (u16, u16),
    scroll_top: u16,
    scroll_bottom: u16,
    style: Style,
    cursor_visible: bool,
    insert_mode: bool,
    origin_mode: bool,
    auto_wrap: bool,
    application_cursor: bool,
    bracketed_paste: bool,
    mouse_tracking: u8,
    mouse_sgr: bool,
    responses: Vec<u8>,
    editor_osc_enabled: bool,
    editor_actions: VecDeque<Vec<u8>>,
    editor_action_bytes: usize,
    shell_command_pending: bool,
    command_completions: VecDeque<i32>,
}

impl Screen {
    fn new(rows: u16, cols: u16, editor_osc_enabled: bool) -> Self {
        Self {
            rows,
            cols,
            cells: blank_rows(rows, cols, Style::default()),
            scrollback: VecDeque::new(),
            alternate: None,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: (0, 0),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            style: Style::default(),
            cursor_visible: true,
            insert_mode: false,
            origin_mode: false,
            auto_wrap: true,
            application_cursor: false,
            bracketed_paste: false,
            mouse_tracking: 0,
            mouse_sgr: false,
            responses: Vec::new(),
            editor_osc_enabled,
            editor_actions: VecDeque::new(),
            editor_action_bytes: 0,
            shell_command_pending: false,
            command_completions: VecDeque::new(),
        }
    }

    fn capture_editor_action(&mut self, encoded: &[u8]) {
        if !self.editor_osc_enabled
            || encoded.len() > MAX_EDITOR_OSC_JSON_BYTES.saturating_mul(2)
            || !encoded.len().is_multiple_of(2)
            || self.editor_actions.len() >= MAX_EDITOR_OSC_ACTIONS
        {
            return;
        }
        let Some(decoded) = decode_hex(encoded) else {
            return;
        };
        if self.editor_action_bytes.saturating_add(decoded.len()) > MAX_EDITOR_OSC_QUEUE_BYTES {
            return;
        }
        self.editor_action_bytes = self.editor_action_bytes.saturating_add(decoded.len());
        self.editor_actions.push_back(decoded);
    }

    fn take_editor_actions(&mut self) -> Vec<Vec<u8>> {
        self.editor_action_bytes = 0;
        self.editor_actions.drain(..).collect()
    }

    fn capture_command_completion(&mut self, encoded: &[u8]) {
        if !self.shell_command_pending {
            return;
        }
        let Ok(text) = std::str::from_utf8(encoded) else {
            return;
        };
        let Ok(status) = text.parse::<i32>() else {
            return;
        };
        self.shell_command_pending = false;
        if self.command_completions.len() < 32 {
            self.command_completions.push_back(status);
        }
    }

    fn record_submitted_input(&mut self, bytes: &[u8]) {
        self.shell_command_pending |= bytes.iter().any(|byte| matches!(byte, b'\r' | b'\n'));
    }

    fn take_command_completions(&mut self) -> Vec<i32> {
        self.command_completions.drain(..).collect()
    }

    fn reset(&mut self) {
        let rows = self.rows;
        let cols = self.cols;
        let editor_osc_enabled = self.editor_osc_enabled;
        let editor_actions = std::mem::take(&mut self.editor_actions);
        let editor_action_bytes = self.editor_action_bytes;
        let shell_command_pending = self.shell_command_pending;
        let command_completions = std::mem::take(&mut self.command_completions);
        *self = Self::new(rows, cols, editor_osc_enabled);
        self.editor_actions = editor_actions;
        self.editor_action_bytes = editor_action_bytes;
        self.shell_command_pending = shell_command_pending;
        self.command_completions = command_completions;
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let next = resize_rows(&self.cells, rows, cols);
        for row in &mut self.scrollback {
            *row = resize_row(row, cols);
        }
        if let Some((main, _, _)) = self.alternate.as_mut() {
            *main = resize_rows(main, rows, cols);
        }
        self.rows = rows;
        self.cols = cols;
        self.cells = next;
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
        self.saved_cursor.0 = self.saved_cursor.0.min(rows.saturating_sub(1));
        self.saved_cursor.1 = self.saved_cursor.1.min(cols.saturating_sub(1));
        if let Some((_, row, col)) = self.alternate.as_mut() {
            *row = (*row).min(rows.saturating_sub(1));
            *col = (*col).min(cols.saturating_sub(1));
        }
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
    }

    fn max_viewport_offset(&self) -> usize {
        if self.alternate.is_some() {
            0
        } else {
            self.scrollback.len()
        }
    }

    fn visible_row(&self, viewport_offset: usize, row: u16) -> &[Cell] {
        let viewport_offset = viewport_offset.min(self.max_viewport_offset());
        let history_start = self.scrollback.len().saturating_sub(viewport_offset);
        let absolute_row = history_start.saturating_add(usize::from(row));
        if absolute_row < self.scrollback.len() {
            return &self.scrollback[absolute_row];
        }
        let live_row = absolute_row.saturating_sub(self.scrollback.len());
        &self.cells[live_row.min(usize::from(self.rows.saturating_sub(1)))]
    }

    fn cursor_row_bounds(&self) -> (u16, u16) {
        if self.origin_mode {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.rows.saturating_sub(1))
        }
    }

    fn home_cursor(&mut self) {
        self.cursor_row = if self.origin_mode { self.scroll_top } else { 0 };
        self.cursor_col = 0;
    }

    fn normalize_row(&mut self, row: u16) {
        normalize_cells(&mut self.cells[usize::from(row)]);
    }

    fn insert_blank_chars(&mut self, count: u16) {
        let count = count.max(1).min(self.cols.saturating_sub(self.cursor_col));
        let row = &mut self.cells[usize::from(self.cursor_row)];
        let start = usize::from(self.cursor_col);
        let end = usize::from(self.cols);
        let count = usize::from(count);
        row[start..end].rotate_right(count);
        for cell in &mut row[start..start + count] {
            reset_cell(cell, self.style);
        }
        self.normalize_row(self.cursor_row);
    }

    fn delete_chars(&mut self, count: u16) {
        let count = count.max(1).min(self.cols.saturating_sub(self.cursor_col));
        let row = &mut self.cells[usize::from(self.cursor_row)];
        let start = usize::from(self.cursor_col);
        let end = usize::from(self.cols);
        let count = usize::from(count);
        row[start..end].rotate_left(count);
        for cell in &mut row[end - count..end] {
            reset_cell(cell, self.style);
        }
        self.normalize_row(self.cursor_row);
    }

    fn erase_chars(&mut self, count: u16) {
        let end = self.cursor_col.saturating_add(count.max(1)).min(self.cols);
        self.clear_row(self.cursor_row, self.cursor_col, end);
        self.normalize_row(self.cursor_row);
    }

    fn insert_lines(&mut self, count: u16) {
        if !(self.scroll_top..=self.scroll_bottom).contains(&self.cursor_row) {
            return;
        }
        let start = usize::from(self.cursor_row);
        let end = usize::from(self.scroll_bottom) + 1;
        let lines = usize::from(count.max(1).min(self.scroll_bottom - self.cursor_row + 1));
        self.cells[start..end].rotate_right(lines);
        for row in &mut self.cells[start..start + lines] {
            reset_row(row, self.style);
        }
    }

    fn delete_lines(&mut self, count: u16) {
        if !(self.scroll_top..=self.scroll_bottom).contains(&self.cursor_row) {
            return;
        }
        let start = usize::from(self.cursor_row);
        let end = usize::from(self.scroll_bottom) + 1;
        let lines = usize::from(count.max(1).min(self.scroll_bottom - self.cursor_row + 1));
        self.cells[start..end].rotate_left(lines);
        for row in &mut self.cells[end - lines..end] {
            reset_row(row, self.style);
        }
    }

    fn clear_row(&mut self, row: u16, start: u16, end: u16) {
        let end = end.min(self.cols);
        for cell in &mut self.cells[usize::from(row)][usize::from(start.min(end))..usize::from(end)]
        {
            reset_cell(cell, self.style);
        }
    }

    fn clear_screen(&mut self) {
        for row in &mut self.cells {
            reset_row(row, Style::default());
        }
    }

    fn newline(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up(1);
        } else {
            self.cursor_row = self
                .cursor_row
                .saturating_add(1)
                .min(self.rows.saturating_sub(1));
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor_row == self.scroll_top {
            self.scroll_down(1);
        } else {
            self.cursor_row = self.cursor_row.saturating_sub(1);
        }
    }

    fn scroll_up(&mut self, count: u16) {
        for _ in 0..count.max(1) {
            let full_screen = self.scroll_top == 0
                && self.scroll_bottom == self.rows.saturating_sub(1)
                && self.alternate.is_none();
            let start = usize::from(self.scroll_top);
            let end = usize::from(self.scroll_bottom);
            let retired = self.cells.remove(start);
            let mut blank = if full_screen {
                let recycled = if self.scrollback.len() >= MAX_SCROLLBACK {
                    self.scrollback.pop_front()
                } else {
                    None
                };
                self.scrollback.push_back(retired);
                recycled.unwrap_or_else(|| vec![Cell::blank(self.style); usize::from(self.cols)])
            } else {
                retired
            };
            reset_row(&mut blank, self.style);
            self.cells.insert(end, blank);
        }
    }

    fn scroll_down(&mut self, count: u16) {
        for _ in 0..count.max(1) {
            let start = usize::from(self.scroll_top);
            let end = usize::from(self.scroll_bottom);
            let mut retired = self.cells.remove(end);
            reset_row(&mut retired, self.style);
            self.cells.insert(start, retired);
        }
    }

    fn put_char(&mut self, character: char) {
        let width = UnicodeWidthChar::width(character).unwrap_or(1);
        if width == 0 {
            let row = &mut self.cells[usize::from(self.cursor_row)];
            let Some(mut col) = usize::from(self.cursor_col).checked_sub(1) else {
                return;
            };
            if row[col].continuation {
                let Some(lead) = col.checked_sub(1) else {
                    return;
                };
                col = lead;
            }
            if !row[col].continuation {
                row[col].text.push(character);
            }
            return;
        }
        if self.cursor_col >= self.cols || (width == 2 && self.cursor_col + 1 >= self.cols) {
            if self.auto_wrap {
                self.cursor_col = 0;
                self.newline();
            } else {
                self.cursor_col = self.cols.saturating_sub(width as u16);
            }
        }
        if self.insert_mode {
            self.insert_blank_chars(width as u16);
        }
        let row = &mut self.cells[usize::from(self.cursor_row)];
        let col = usize::from(self.cursor_col);
        clear_glyph_at(row, col);
        if width == 2 {
            clear_glyph_at(row, col + 1);
        }
        row[col] = Cell {
            text: character.to_string(),
            continuation: false,
            style: self.style,
        };
        if width == 2 && self.cursor_col + 1 < self.cols {
            row[col + 1] = Cell {
                text: String::new(),
                continuation: true,
                style: self.style,
            };
        }
        self.cursor_col = self.cursor_col.saturating_add(width as u16);
    }

    fn enter_alternate(&mut self) {
        if self.alternate.is_some() {
            return;
        }
        let main = std::mem::replace(
            &mut self.cells,
            blank_rows(self.rows, self.cols, Style::default()),
        );
        self.alternate = Some((main, self.cursor_row, self.cursor_col));
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
    }

    fn leave_alternate(&mut self) {
        if let Some((main, row, col)) = self.alternate.take() {
            self.cells = main;
            self.cursor_row = row.min(self.rows.saturating_sub(1));
            self.cursor_col = col.min(self.cols.saturating_sub(1));
            self.scroll_top = 0;
            self.scroll_bottom = self.rows.saturating_sub(1);
        }
    }

    fn params(params: &Params) -> Vec<u16> {
        params
            .iter()
            .map(|parameter| parameter.first().copied().unwrap_or(0))
            .collect()
    }

    fn parameter(values: &[u16], index: usize, default: u16) -> u16 {
        values
            .get(index)
            .copied()
            .filter(|value| *value != 0)
            .unwrap_or(default)
    }

    fn set_sgr(&mut self, values: &[u16]) {
        if values.is_empty() {
            self.style = Style::default();
            return;
        }
        let mut index = 0;
        while index < values.len() {
            match values[index] {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                2 => self.style.faint = true,
                3 => self.style.italic = true,
                4 | 21 => self.style.underline = true,
                7 => self.style.inverse = true,
                9 => self.style.strikethrough = true,
                22 => {
                    self.style.bold = false;
                    self.style.faint = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.inverse = false,
                29 => self.style.strikethrough = false,
                30..=37 => self.style.fg = ansi_color(values[index] - 30, false),
                39 => self.style.fg = DEFAULT_FG,
                40..=47 => self.style.bg = ansi_color(values[index] - 40, false),
                49 => self.style.bg = DEFAULT_BG,
                90..=97 => self.style.fg = ansi_color(values[index] - 90, true),
                100..=107 => self.style.bg = ansi_color(values[index] - 100, true),
                38 | 48 if values.get(index + 1) == Some(&5) => {
                    if let Some(value) = values.get(index + 2) {
                        let color = indexed_color(*value);
                        if values[index] == 38 {
                            self.style.fg = color;
                        } else {
                            self.style.bg = color;
                        }
                    }
                    index += 2;
                }
                38 | 48 if values.get(index + 1) == Some(&2) => {
                    if let (Some(red), Some(green), Some(blue)) = (
                        values.get(index + 2),
                        values.get(index + 3),
                        values.get(index + 4),
                    ) {
                        let color = (u32::from(*red).min(255) << 16)
                            | (u32::from(*green).min(255) << 8)
                            | u32::from(*blue).min(255);
                        if values[index] == 38 {
                            self.style.fg = color;
                        } else {
                            self.style.bg = color;
                        }
                    }
                    index += 4;
                }
                _ => {}
            }
            index += 1;
        }
    }
}

fn blank_rows(rows: u16, cols: u16, style: Style) -> Vec<Vec<Cell>> {
    (0..rows)
        .map(|_| vec![Cell::blank(style); usize::from(cols)])
        .collect()
}

fn reset_cell(cell: &mut Cell, style: Style) {
    cell.text.clear();
    cell.text.push(' ');
    cell.continuation = false;
    cell.style = style;
}

fn reset_row(row: &mut [Cell], style: Style) {
    for cell in row {
        reset_cell(cell, style);
    }
}

fn clear_glyph_at(row: &mut [Cell], col: usize) {
    if col >= row.len() {
        return;
    }
    let lead = if row[col].continuation {
        col.saturating_sub(1)
    } else {
        col
    };
    let width = row[lead]
        .text
        .chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(1))
        .sum::<usize>();
    let style = row[lead].style;
    reset_cell(&mut row[lead], style);
    if width == 2 && lead + 1 < row.len() {
        let continuation_style = row[lead + 1].style;
        reset_cell(&mut row[lead + 1], continuation_style);
    }
}

fn resize_rows(cells: &[Vec<Cell>], rows: u16, cols: u16) -> Vec<Vec<Cell>> {
    let mut next = blank_rows(rows, cols, Style::default());
    let copy_rows = usize::from(rows).min(cells.len());
    for row in 0..copy_rows {
        next[row] = resize_row(&cells[row], cols);
    }
    next
}

fn resize_row(row: &[Cell], cols: u16) -> Vec<Cell> {
    let mut next = row
        .iter()
        .take(usize::from(cols))
        .cloned()
        .collect::<Vec<_>>();
    next.resize_with(usize::from(cols), || Cell::blank(Style::default()));
    normalize_cells(&mut next);
    next
}

/// Restore the invariant shared by live, alternate, and scrollback rows: a row
/// has no orphan continuation cells and every two-cell glyph owns exactly the
/// following cell. Resizing can split a wide glyph at the right edge, while
/// insertion/deletion can move only one half of a glyph.
fn normalize_cells(row: &mut [Cell]) {
    let mut col = 0;
    while col < row.len() {
        if row[col].continuation {
            let style = row[col].style;
            reset_cell(&mut row[col], style);
            col += 1;
            continue;
        }

        let width = row[col]
            .text
            .chars()
            .map(|character| UnicodeWidthChar::width(character).unwrap_or(1))
            .sum::<usize>();
        if width == 2 {
            if col + 1 == row.len() {
                let style = row[col].style;
                reset_cell(&mut row[col], style);
            } else {
                row[col + 1] = Cell {
                    text: String::new(),
                    continuation: true,
                    style: row[col].style,
                };
                col += 2;
                continue;
            }
        }
        col += 1;
    }
}

impl Perform for Screen {
    fn print(&mut self, character: char) {
        self.put_char(character);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.newline(),
            b'\r' => self.cursor_col = 0,
            0x08 => self.cursor_col = self.cursor_col.saturating_sub(1),
            b'\t' => {
                self.cursor_col = ((self.cursor_col / 8) + 1)
                    .saturating_mul(8)
                    .min(self.cols.saturating_sub(1))
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        match params {
            [b"11", b"?"] => {
                let red = (DEFAULT_BG >> 16) & 0xff;
                let green = (DEFAULT_BG >> 8) & 0xff;
                let blue = DEFAULT_BG & 0xff;
                self.responses.extend_from_slice(
                    format!(
                        "\x1b]11;rgb:{red:02x}{red:02x}/{green:02x}{green:02x}/{blue:02x}{blue:02x}\x1b\\"
                    )
                    .as_bytes(),
                );
            }
            [b"777", b"brood", encoded] => self.capture_editor_action(encoded),
            [b"777", b"brood-inbox", status] => self.capture_command_completion(status),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let values = Self::params(params);
        let first = Self::parameter(&values, 0, 1);
        let private = intermediates.contains(&b'?');
        match action {
            'A' => {
                let (top, _) = self.cursor_row_bounds();
                self.cursor_row = self.cursor_row.saturating_sub(first).max(top);
            }
            'B' => {
                let (_, bottom) = self.cursor_row_bounds();
                self.cursor_row = self.cursor_row.saturating_add(first).min(bottom);
            }
            'C' => {
                self.cursor_col = self
                    .cursor_col
                    .saturating_add(first)
                    .min(self.cols.saturating_sub(1));
            }
            'D' => self.cursor_col = self.cursor_col.saturating_sub(first),
            'E' => {
                let (_, bottom) = self.cursor_row_bounds();
                self.cursor_row = self.cursor_row.saturating_add(first).min(bottom);
                self.cursor_col = 0;
            }
            'F' => {
                let (top, _) = self.cursor_row_bounds();
                self.cursor_row = self.cursor_row.saturating_sub(first).max(top);
                self.cursor_col = 0;
            }
            'G' | '`' => self.cursor_col = first.saturating_sub(1).min(self.cols.saturating_sub(1)),
            'd' => {
                let (top, bottom) = self.cursor_row_bounds();
                self.cursor_row = top.saturating_add(first.saturating_sub(1)).min(bottom);
            }
            'H' | 'f' => {
                let (top, bottom) = self.cursor_row_bounds();
                self.cursor_row = top.saturating_add(first.saturating_sub(1)).min(bottom);
                self.cursor_col = Self::parameter(&values, 1, 1)
                    .saturating_sub(1)
                    .min(self.cols.saturating_sub(1));
            }
            'a' => {
                self.cursor_col = self
                    .cursor_col
                    .saturating_add(first)
                    .min(self.cols.saturating_sub(1));
            }
            'e' => {
                let (_, bottom) = self.cursor_row_bounds();
                self.cursor_row = self.cursor_row.saturating_add(first).min(bottom);
            }
            '@' => self.insert_blank_chars(first),
            'P' => self.delete_chars(first),
            'X' => self.erase_chars(first),
            'L' => self.insert_lines(first),
            'M' => self.delete_lines(first),
            'J' => match values.first().copied().unwrap_or(0) {
                2 => self.clear_screen(),
                3 => self.scrollback.clear(),
                1 => {
                    for row in 0..self.cursor_row {
                        self.clear_row(row, 0, self.cols);
                    }
                    self.clear_row(self.cursor_row, 0, self.cursor_col.saturating_add(1));
                }
                _ => {
                    self.clear_row(self.cursor_row, self.cursor_col, self.cols);
                    for row in self.cursor_row.saturating_add(1)..self.rows {
                        self.clear_row(row, 0, self.cols);
                    }
                }
            },
            'K' => match values.first().copied().unwrap_or(0) {
                1 => self.clear_row(self.cursor_row, 0, self.cursor_col.saturating_add(1)),
                2 => self.clear_row(self.cursor_row, 0, self.cols),
                _ => self.clear_row(self.cursor_row, self.cursor_col, self.cols),
            },
            'm' => self.set_sgr(&values),
            's' => self.saved_cursor = (self.cursor_row, self.cursor_col),
            'u' if !private => (self.cursor_row, self.cursor_col) = self.saved_cursor,
            'r' => {
                let top = first.saturating_sub(1).min(self.rows.saturating_sub(1));
                let bottom = Self::parameter(&values, 1, self.rows)
                    .saturating_sub(1)
                    .min(self.rows.saturating_sub(1));
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.home_cursor();
                }
            }
            'S' => self.scroll_up(first),
            'T' => self.scroll_down(first),
            'h' if !private => {
                if values.contains(&4) {
                    self.insert_mode = true;
                }
            }
            'l' if !private => {
                if values.contains(&4) {
                    self.insert_mode = false;
                }
            }
            'h' if private => {
                for mode in values {
                    match mode {
                        1 => self.application_cursor = true,
                        6 => {
                            self.origin_mode = true;
                            self.home_cursor();
                        }
                        7 => self.auto_wrap = true,
                        25 => self.cursor_visible = true,
                        47 | 1047 | 1049 => self.enter_alternate(),
                        1048 => self.saved_cursor = (self.cursor_row, self.cursor_col),
                        1000 => self.mouse_tracking = 1,
                        1002 => self.mouse_tracking = 2,
                        1003 => self.mouse_tracking = 3,
                        1006 => self.mouse_sgr = true,
                        2004 => self.bracketed_paste = true,
                        _ => {}
                    }
                }
            }
            'l' if private => {
                for mode in values {
                    match mode {
                        1 => self.application_cursor = false,
                        6 => {
                            self.origin_mode = false;
                            self.home_cursor();
                        }
                        7 => self.auto_wrap = false,
                        25 => self.cursor_visible = false,
                        47 | 1047 | 1049 => self.leave_alternate(),
                        1048 => (self.cursor_row, self.cursor_col) = self.saved_cursor,
                        1000 if self.mouse_tracking == 1 => self.mouse_tracking = 0,
                        1002 if self.mouse_tracking == 2 => self.mouse_tracking = 0,
                        1003 if self.mouse_tracking == 3 => self.mouse_tracking = 0,
                        1006 => self.mouse_sgr = false,
                        2004 => self.bracketed_paste = false,
                        _ => {}
                    }
                }
            }
            'n' if values.first() == Some(&6) => {
                self.responses.extend_from_slice(
                    format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_col + 1).as_bytes(),
                );
            }
            'c' => self.responses.extend_from_slice(b"\x1b[?1;2c"),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => self.saved_cursor = (self.cursor_row, self.cursor_col),
            b'8' => (self.cursor_row, self.cursor_col) = self.saved_cursor,
            b'D' => self.newline(),
            b'E' => {
                self.cursor_col = 0;
                self.newline();
            }
            b'M' => self.reverse_index(),
            b'c' => self.reset(),
            _ => {}
        }
    }
}

fn ansi_color(index: u16, bright: bool) -> u32 {
    let index = usize::from(index.min(7));
    if bright {
        ashen::ANSI_BRIGHT[index]
    } else {
        ashen::ANSI_NORMAL[index]
    }
}

fn indexed_color(index: u16) -> u32 {
    if index < 8 {
        return ansi_color(index, false);
    }
    if index < 16 {
        return ansi_color(index - 8, true);
    }
    if index < 232 {
        let value = index - 16;
        let red = value / 36;
        let green = (value % 36) / 6;
        let blue = value % 6;
        let channel = |component: u16| -> u32 {
            if component == 0 {
                0
            } else {
                u32::from(component) * 40 + 55
            }
        };
        return (channel(red) << 16) | (channel(green) << 8) | channel(blue);
    }
    let gray = u32::from(index.min(255) - 232) * 10 + 8;
    (gray << 16) | (gray << 8) | gray
}

fn encode_snapshot(state: &mut SessionState) -> Vec<Vec<u8>> {
    let mut row_blobs = Vec::with_capacity(usize::from(state.screen.rows));
    for row in 0..state.screen.rows {
        row_blobs.push(encode_row(&state.screen, state.viewport_offset, row));
    }
    let full = state.force_full_snapshot || state.published_rows.len() != row_blobs.len();
    let changed_rows = row_blobs
        .iter()
        .enumerate()
        .filter(|(row, blob)| full || state.published_rows.get(*row) != Some(*blob))
        .map(|(row, blob)| (row as u16, blob.clone()))
        .collect::<Vec<_>>();
    state.published_rows = row_blobs;
    state.force_full_snapshot = false;

    let mut groups = Vec::<(u16, Vec<Vec<u8>>)>::new();
    let mut start = 0_u16;
    let mut current = Vec::new();
    let mut current_bytes = 81_usize;
    for (row, blob) in changed_rows {
        let contiguous = row == start.saturating_add(current.len() as u16);
        if !current.is_empty()
            && (!contiguous || current_bytes.saturating_add(blob.len()) > MAX_SNAPSHOT_BODY)
        {
            groups.push((start, std::mem::take(&mut current)));
            current_bytes = 81;
        }
        if current.is_empty() {
            start = row;
        }
        current_bytes = current_bytes.saturating_add(blob.len());
        current.push(blob);
    }
    groups.push((start, current));
    let chunk_count = groups.len().min(u16::MAX as usize) as u16;
    groups
        .into_iter()
        .enumerate()
        .map(|(index, (row_start, rows))| {
            let mut bytes = Vec::with_capacity(80 + rows.iter().map(Vec::len).sum::<usize>());
            bytes.extend_from_slice(SNAPSHOT_MAGIC);
            bytes.push(SNAPSHOT_VERSION);
            bytes.push(if full { SNAPSHOT_FULL } else { 0 });
            bytes.extend_from_slice(&state.revision.to_le_bytes());
            bytes.extend_from_slice(&state.snapshot_input_sequence.to_le_bytes());
            bytes.extend_from_slice(&state.screen.rows.to_le_bytes());
            bytes.extend_from_slice(&state.screen.cols.to_le_bytes());
            bytes.extend_from_slice(&state.screen.cursor_row.to_le_bytes());
            bytes.extend_from_slice(&state.screen.cursor_col.to_le_bytes());
            bytes.push(u8::from(
                state.screen.cursor_visible && state.viewport_offset == 0,
            ));
            bytes.push(
                u8::from(state.screen.application_cursor)
                    | (u8::from(state.screen.bracketed_paste) << 1)
                    | (state.screen.mouse_tracking << 2)
                    | (u8::from(state.screen.mouse_sgr) << 4),
            );
            bytes.push(u8::from(state.exited));
            bytes.extend_from_slice(&state.exit_code.unwrap_or(i32::MIN).to_le_bytes());
            bytes.extend_from_slice(&(index as u16).to_le_bytes());
            bytes.extend_from_slice(&chunk_count.to_le_bytes());
            bytes.extend_from_slice(&row_start.to_le_bytes());
            bytes.extend_from_slice(&(rows.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&state.chunks_read.to_le_bytes());
            bytes.extend_from_slice(&state.bytes_read.to_le_bytes());
            bytes.extend_from_slice(&state.resizes.to_le_bytes());
            bytes.extend_from_slice(
                &u32::try_from(state.viewport_offset)
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            bytes.extend_from_slice(
                &u32::try_from(state.screen.max_viewport_offset())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for row in rows {
                bytes.extend_from_slice(&row);
            }
            bytes
        })
        .collect()
}

fn encode_row(screen: &Screen, viewport_offset: usize, row: u16) -> Vec<u8> {
    let cells = screen.visible_row(viewport_offset, row);
    let mut bytes = Vec::with_capacity(cells.len().saturating_mul(2));
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    let mut run_count = 0_u16;
    let mut start = 0_u16;
    let mut style = cells.first().map_or(Style::default(), |cell| cell.style);
    let mut run = begin_encoded_run(&mut bytes, start, style);
    for (col, cell) in cells.iter().enumerate() {
        if cell.style != style {
            finish_encoded_run(&mut bytes, run, col as u16 - start);
            run_count = run_count.saturating_add(1);
            start = col as u16;
            style = cell.style;
            run = begin_encoded_run(&mut bytes, start, style);
        }
        if !cell.continuation {
            let text_bytes = cell.text.as_bytes();
            let remaining = (u16::MAX as usize).saturating_sub(bytes.len() - run.text_start);
            bytes.extend_from_slice(&text_bytes[..text_bytes.len().min(remaining)]);
        }
    }
    finish_encoded_run(&mut bytes, run, screen.cols - start);
    run_count = run_count.saturating_add(1);
    bytes[..2].copy_from_slice(&run_count.to_le_bytes());
    bytes
}

struct EncodedRun {
    cells_offset: usize,
    text_length_offset: usize,
    text_start: usize,
}

fn begin_encoded_run(bytes: &mut Vec<u8>, start: u16, style: Style) -> EncodedRun {
    let (fg, bg) = if style.inverse {
        (style.bg, style.fg)
    } else {
        (style.fg, style.bg)
    };
    bytes.extend_from_slice(&start.to_le_bytes());
    let cells_offset = bytes.len();
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&fg.to_le_bytes());
    bytes.extend_from_slice(&bg.to_le_bytes());
    bytes.push(
        u8::from(style.bold)
            | (u8::from(style.faint) << 1)
            | (u8::from(style.italic) << 2)
            | (u8::from(style.underline) << 3)
            | (u8::from(style.strikethrough) << 4),
    );
    let text_length_offset = bytes.len();
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    EncodedRun {
        cells_offset,
        text_length_offset,
        text_start: bytes.len(),
    }
}

fn finish_encoded_run(bytes: &mut [u8], run: EncodedRun, cells: u16) {
    bytes[run.cells_offset..run.cells_offset + 2].copy_from_slice(&cells.to_le_bytes());
    let text_length = (bytes.len() - run.text_start) as u16;
    bytes[run.text_length_offset..run.text_length_offset + 2]
        .copy_from_slice(&text_length.to_le_bytes());
}

bindings::export!(TerminalSession with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::{
        Cell, EditorOpen, MAX_PENDING_EDITOR_CONTROLS, SNAPSHOT_FULL, SNAPSHOT_VERSION, Screen,
        SessionState, Style, decode_editor_control, editor_cursor_arguments,
        editor_remote_arguments, editor_socket_path, editor_worker_finished_message, encode_row,
        encode_snapshot, is_editor_session_instance, parse_editor_open, route_editor_action,
        trusted_input_sender,
    };
    use brood_plugin_sdk::exports::brood::plugin::actor::Command;
    use serde_json::Value;
    use vte::{Parser, Perform};

    fn row_range(chunk: &[u8]) -> (u16, u16) {
        (
            u16::from_le_bytes([chunk[41], chunk[42]]),
            u16::from_le_bytes([chunk[43], chunk[44]]),
        )
    }

    fn encoded_row_runs(row: &[u8]) -> Vec<(u16, u16, Style, String)> {
        let mut offset = 0;
        let take_u16 = |bytes: &[u8], offset: &mut usize| {
            let value = u16::from_le_bytes([bytes[*offset], bytes[*offset + 1]]);
            *offset += 2;
            value
        };
        let take_u32 = |bytes: &[u8], offset: &mut usize| {
            let value =
                u32::from_le_bytes(bytes[*offset..*offset + 4].try_into().expect("encoded u32"));
            *offset += 4;
            value
        };
        let count = take_u16(row, &mut offset);
        (0..count)
            .map(|_| {
                let start = take_u16(row, &mut offset);
                let cells = take_u16(row, &mut offset);
                let fg = take_u32(row, &mut offset);
                let bg = take_u32(row, &mut offset);
                let flags = row[offset];
                offset += 1;
                let text_len = usize::from(take_u16(row, &mut offset));
                let text = std::str::from_utf8(&row[offset..offset + text_len])
                    .expect("encoded row UTF-8")
                    .to_string();
                offset += text_len;
                (
                    start,
                    cells,
                    Style {
                        fg,
                        bg,
                        bold: flags & 1 != 0,
                        faint: flags & 2 != 0,
                        italic: flags & 4 != 0,
                        underline: flags & 8 != 0,
                        strikethrough: flags & 16 != 0,
                        inverse: false,
                    },
                    text,
                )
            })
            .collect()
    }

    #[test]
    fn width_resize_normalizes_styled_live_alternate_and_scrollback_rows() {
        let styled = Style {
            fg: 0x12_34_56,
            bg: 0x65_43_21,
            bold: true,
            ..Style::default()
        };
        let wide_row = || {
            let mut row = vec![Cell::blank(styled); 6];
            row[0].text = "h".to_string();
            row[4].text = "界".to_string();
            row[5] = Cell {
                text: String::new(),
                continuation: true,
                style: styled,
            };
            row
        };
        let mut screen = Screen::new(2, 6, false);
        screen.scrollback.push_back(wide_row());
        screen.cells[0] = wide_row();
        screen.enter_alternate();
        screen.cells[0] = wide_row();

        screen.resize(2, 5);
        for row in [
            &screen.scrollback[0],
            &screen.cells[0],
            &screen.alternate.as_ref().unwrap().0[0],
        ] {
            assert_eq!(row.len(), 5);
            assert_eq!(row[0].style, styled);
            assert_eq!(row[4], Cell::blank(styled));
            assert!(!row.iter().any(|cell| cell.continuation));
        }

        screen.resize(2, 8);
        for row in [
            &screen.scrollback[0],
            &screen.cells[0],
            &screen.alternate.as_ref().unwrap().0[0],
        ] {
            assert_eq!(row.len(), 8);
            assert_eq!(row[0].style, styled);
            assert_eq!(row[4], Cell::blank(styled));
            assert_eq!(row[7], Cell::blank(Style::default()));
        }
    }

    #[test]
    fn resized_scrollback_encodes_only_runs_within_the_new_width() {
        let mut screen = Screen::new(2, 7, false);
        let mut history = vec![Cell::blank(Style::default()); 7];
        history[0].text = "o".to_string();
        history[0].style.bold = true;
        history[1].style.bold = true;
        history[5].style.bold = true;
        history[6].style.bold = true;
        screen.scrollback.push_back(history);

        screen.resize(2, 3);
        let encoded = encode_row(&screen, 1, 0);
        let runs = encoded_row_runs(&encoded);
        assert!(!runs.is_empty());
        assert!(
            runs.iter()
                .all(|(start, cells, _, _)| start.saturating_add(*cells) <= 3)
        );
        assert_eq!(runs.iter().map(|(_, cells, _, _)| cells).sum::<u16>(), 3);
        assert!(runs.iter().any(|(_, _, _, text)| text.starts_with('o')));
        assert!(runs.iter().any(|(_, _, style, _)| style.bold));
    }

    #[test]
    fn combining_mark_after_wide_glyph_belongs_to_the_lead_cell_and_snapshot() {
        let mut screen = Screen::new(2, 6, false);
        let mut parser = Parser::new();
        parser.advance(&mut screen, "界\u{301}".as_bytes());

        assert_eq!(screen.cursor_col, 2);
        assert_eq!(screen.cells[0][0].text, "界\u{301}");
        assert!(screen.cells[0][1].continuation);
        assert!(screen.cells[0][1].text.is_empty());

        let runs = encoded_row_runs(&encode_row(&screen, 0, 0));
        assert!(
            runs.iter()
                .any(|(_, _, _, text)| text.starts_with("界\u{301}"))
        );
    }

    #[test]
    fn snapshots_bootstrap_fully_then_send_only_changed_rows() {
        let mut state = SessionState::new(1, 1, false, None);
        let initial = encode_snapshot(&mut state);
        assert_eq!(initial[0][4], SNAPSHOT_VERSION);
        assert_eq!(initial[0][5], SNAPSHOT_FULL);
        assert_eq!(row_range(&initial[0]), (0, 24));

        state.revision += 1;
        let unchanged = encode_snapshot(&mut state);
        assert_eq!(unchanged.len(), 1);
        assert_eq!(unchanged[0][5], 0);
        assert_eq!(row_range(&unchanged[0]), (0, 0));

        state.screen.cells[2][3].text = "x".to_string();
        state.revision += 1;
        let changed = encode_snapshot(&mut state);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0][5], 0);
        assert_eq!(row_range(&changed[0]), (2, 1));

        state.force_full_snapshot = true;
        state.revision += 1;
        let replacement = encode_snapshot(&mut state);
        assert_eq!(replacement[0][5], SNAPSHOT_FULL);
        assert_eq!(row_range(&replacement[0]), (0, 24));
    }

    #[test]
    fn snapshot_acknowledges_only_fully_written_terminal_inputs() -> Result<(), String> {
        let mut state = SessionState::new(1, 1, false, None);
        state.queue_input(7, b"ab");
        state.queue_input(8, b"c");
        state.record_written_input(2);
        state.snapshot_input_sequence = state.last_written_input_sequence;
        let first = encode_snapshot(&mut state);
        let first_ack = first[0][14..22]
            .try_into()
            .map_err(|_| "first snapshot acknowledgement is truncated")?;
        assert_eq!(u64::from_le_bytes(first_ack), 7);

        state.record_written_input(1);
        state.snapshot_input_sequence = state.last_written_input_sequence;
        let second = encode_snapshot(&mut state);
        let second_ack = second[0][14..22]
            .try_into()
            .map_err(|_| "second snapshot acknowledgement is truncated")?;
        assert_eq!(u64::from_le_bytes(second_ack), 8);
        Ok(())
    }

    #[test]
    fn editor_socket_names_are_short_and_generation_unique() {
        let first = editor_socket_path(7, 11, "neovim-session");
        let second = editor_socket_path(7, 12, "neovim-session");
        assert_ne!(first, second);
        assert!(first.starts_with("/tmp/brood-nvim-s7-"));
        assert!(first.len() < 100);
    }

    #[test]
    fn editor_role_accepts_realm_scoped_instance_id() {
        assert!(is_editor_session_instance("neovim-session"));
        assert!(is_editor_session_instance("brood.session.7.neovim-session"));
        assert!(!is_editor_session_instance("terminal-session"));
        assert!(!is_editor_session_instance("not-neovim-session"));
    }

    #[test]
    fn editor_exit_before_readiness_or_with_failure_is_not_clean() {
        let mut state =
            SessionState::new(7, 1, true, Some("/tmp/brood-nvim-test.sock".to_string()));
        state.exit_code = Some(0);
        let Command::SendLocalMessage(message) =
            editor_worker_finished_message(&state).expect("editor lifecycle message")
        else {
            panic!("editor completion must use the local init mailbox");
        };
        let body: Value = serde_json::from_slice(&message.body).expect("lifecycle JSON");
        assert_eq!(body["kind"], "editor-failed");

        state.editor_ready = true;
        state.exit_code = Some(1);
        let Command::SendLocalMessage(message) =
            editor_worker_finished_message(&state).expect("editor lifecycle message")
        else {
            panic!("editor completion must use the local init mailbox");
        };
        let body: Value = serde_json::from_slice(&message.body).expect("lifecycle JSON");
        assert_eq!(body["kind"], "editor-failed");

        state.exit_code = Some(0);
        let Command::SendLocalMessage(message) =
            editor_worker_finished_message(&state).expect("editor lifecycle message")
        else {
            panic!("editor completion must use the local init mailbox");
        };
        let body: Value = serde_json::from_slice(&message.body).expect("lifecycle JSON");
        assert_eq!(body["kind"], "editor-stopped");
    }

    #[test]
    fn editor_open_is_typed_bounded_and_builds_remote_arguments() {
        let request = parse_editor_open(
            br#"{"kind":"editor-open","path":"/work/src/main.rs","line":12,"column":4}"#,
        )
        .expect("valid editor-open");
        assert_eq!(
            request,
            EditorOpen {
                path: "/work/src/main.rs".to_string(),
                line: Some(12),
                column: Some(4),
            }
        );
        assert_eq!(
            editor_remote_arguments("/tmp/editor.sock", &request),
            vec![
                "--server",
                "/tmp/editor.sock",
                "--remote",
                "/work/src/main.rs",
            ]
        );
        assert_eq!(
            editor_cursor_arguments("/tmp/editor.sock", &request),
            Some(vec![
                "--server".to_string(),
                "/tmp/editor.sock".to_string(),
                "--remote-expr".to_string(),
                "cursor(12,4)".to_string(),
            ])
        );
        assert!(parse_editor_open(br#"{"kind":"editor-open","path":"","line":1}"#).is_none());
        assert!(parse_editor_open(br#"{"kind":"editor-open","path":"/tmp/a","line":0}"#).is_none());
        assert_eq!(
            editor_remote_arguments(
                "/tmp/editor.sock",
                &EditorOpen {
                    path: "-option-like".to_string(),
                    line: None,
                    column: None,
                }
            ),
            vec!["--server", "/tmp/editor.sock", "--remote", "./-option-like",]
        );
    }

    #[test]
    fn editor_controls_are_serial_and_bounded() {
        let mut state =
            SessionState::new(7, 1, true, Some("/tmp/brood-nvim-test.sock".to_string()));
        state.editor_ready = true;
        let request = EditorOpen {
            path: "/tmp/a".to_string(),
            line: Some(3),
            column: None,
        };
        let first = state.queue_editor_open(request.clone());
        assert!(matches!(first.as_slice(), [Command::SpawnWorker(_)]));
        for _ in 1..MAX_PENDING_EDITOR_CONTROLS {
            assert!(state.queue_editor_open(request.clone()).is_empty());
        }
        let rejected = state.queue_editor_open(request);
        assert!(matches!(
            rejected.as_slice(),
            [Command::SendPresentationMessage(message)]
                if message.actor == super::CLIENT_INIT_ID
        ));
        assert_eq!(
            state.pending_editor_controls.len(),
            MAX_PENDING_EDITOR_CONTROLS - 1
        );
        let payload = match &first[0] {
            Command::SpawnWorker(request) => &request.payload,
            _ => unreachable!(),
        };
        let (socket, decoded) = decode_editor_control(payload).expect("encoded editor control");
        assert_eq!(socket, "/tmp/brood-nvim-test.sock");
        assert_eq!(decoded.path, "/tmp/a");
    }

    #[test]
    fn editor_readiness_waits_for_a_successful_server_probe() {
        let mut state =
            SessionState::new(7, 1, true, Some("/tmp/brood-nvim-test.sock".to_string()));
        state.child = Some(9);
        let queued = state.queue_editor_open(EditorOpen {
            path: "/tmp/a".to_string(),
            line: None,
            column: None,
        });
        assert!(queued.is_empty());
        assert!(!state.editor_ready);
        assert!(matches!(
            state.start_editor_probe(),
            Some(Command::SpawnWorker(request))
                if request.worker == super::EDITOR_PROBE_WORKER
        ));
        let ready = state.finish_editor_probe(None);
        assert!(state.editor_ready);
        assert!(matches!(
            ready.as_slice(),
            [Command::SendLocalMessage(message), Command::SpawnWorker(_)]
                if message.actor == "init"
        ));
    }

    #[test]
    fn private_editor_osc_is_role_gated_and_routes_to_client_init() {
        let json = br#"{"action":"terminal","command":"cargo test","agent":"ignored"}"#;
        let encoded = json
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let params = [b"777".as_slice(), b"brood".as_slice(), encoded.as_bytes()];

        let mut normal = Screen::new(4, 8, false);
        normal.osc_dispatch(&params, false);
        assert!(normal.take_editor_actions().is_empty());

        let mut editor = Screen::new(4, 8, true);
        editor.osc_dispatch(&params, false);
        let captured = editor.take_editor_actions();
        assert_eq!(captured, vec![json.to_vec()]);
        let commands = route_editor_action(44, &captured[0]);
        let Command::SendPresentationMessage(message) = &commands[0] else {
            panic!("editor action did not target the presentation realm");
        };
        assert_eq!(message.actor, "brood.client-init");
        let forwarded: Value =
            serde_json::from_slice(&message.body).expect("forwarded editor action JSON");
        assert_eq!(forwarded["kind"], "editor-action");
        assert_eq!(forwarded["action"], "terminal");
        assert_eq!(forwarded["session_id"], 44);
        assert_eq!(forwarded["command"], "cargo test");
    }

    #[test]
    fn shell_prompt_markers_require_submitted_input_and_queue_command_statuses() {
        let mut screen = Screen::new(4, 8, false);
        screen.osc_dispatch(
            &[
                b"777".as_slice(),
                b"brood-inbox".as_slice(),
                b"0".as_slice(),
            ],
            true,
        );
        assert!(screen.take_command_completions().is_empty());

        screen.osc_dispatch(
            &[
                b"777".as_slice(),
                b"brood-inbox".as_slice(),
                b"0".as_slice(),
            ],
            true,
        );
        assert!(screen.take_command_completions().is_empty());

        screen.record_submitted_input(b"printf ok\r");
        screen.osc_dispatch(
            &[
                b"777".as_slice(),
                b"brood-inbox".as_slice(),
                b"0".as_slice(),
            ],
            true,
        );
        assert_eq!(screen.take_command_completions(), vec![0]);

        screen.osc_dispatch(
            &[
                b"777".as_slice(),
                b"brood-inbox".as_slice(),
                b"9".as_slice(),
            ],
            true,
        );
        assert!(screen.take_command_completions().is_empty());

        screen.record_submitted_input(b"false\n");
        screen.osc_dispatch(
            &[
                b"777".as_slice(),
                b"brood-inbox".as_slice(),
                b"17".as_slice(),
            ],
            true,
        );
        assert_eq!(screen.take_command_completions(), vec![17]);
    }

    #[test]
    fn generic_editor_messages_only_use_actor_message_commands() {
        let local = route_editor_action(
            1,
            br#"{"action":"message","target":"session:terminal","body":"ls"}"#,
        );
        assert!(matches!(
            local.as_slice(),
            [Command::SendLocalMessage(message)]
                if message.actor == "terminal" && message.body == b"ls"
        ));
        let presentation = route_editor_action(
            1,
            br#"{"action":"message","target":"presentation:diff-viewer","body":{"kind":"focus"}}"#,
        );
        assert!(matches!(
            presentation.as_slice(),
            [Command::SendPresentationMessage(message)]
                if message.actor == "diff-viewer"
        ));
        assert!(
            route_editor_action(
                1,
                br#"{"action":"message","target":"presentation:../../host","body":"no"}"#,
            )
            .is_empty()
        );
        for actor in ["brood.client-init", "session-switcher"] {
            let payload = format!(
                r#"{{"action":"message","target":"presentation:{actor}","body":{{"kind":"editor-action","session_id":99}}}}"#
            );
            assert!(route_editor_action(1, payload.as_bytes()).is_empty());
        }
    }

    #[test]
    fn osc_background_query_answers_neovim_and_private_u_does_not_restore_cursor() {
        let mut screen = Screen::new(8, 16, true);
        let mut parser = Parser::new();
        parser.advance(&mut screen, b"\x1b]11;?\x1b\\");
        assert_eq!(
            screen.responses,
            format!(
                "\x1b]11;rgb:{0:02x}{0:02x}/{1:02x}{1:02x}/{2:02x}{2:02x}\x1b\\",
                (super::DEFAULT_BG >> 16) & 0xff,
                (super::DEFAULT_BG >> 8) & 0xff,
                super::DEFAULT_BG & 0xff,
            )
            .into_bytes()
        );

        screen.saved_cursor = (1, 2);
        screen.cursor_row = 5;
        screen.cursor_col = 6;
        parser.advance(&mut screen, b"\x1b[?u");
        assert_eq!((screen.cursor_row, screen.cursor_col), (5, 6));
        parser.advance(&mut screen, b"\x1b[u");
        assert_eq!((screen.cursor_row, screen.cursor_col), (1, 2));
    }

    #[test]
    fn csi_vertical_relative_position_advances_once() {
        let mut screen = Screen::new(8, 16, false);
        let mut parser = Parser::new();
        parser.advance(&mut screen, b"\x1b[2e");
        assert_eq!(screen.cursor_row, 2);
    }

    #[test]
    fn init_and_presentation_can_send_terminal_input() {
        assert!(trusted_input_sender("init"));
        assert!(trusted_input_sender("presentation:terminal-ui"));
        assert!(!trusted_input_sender("other-session-actor"));
    }
}
