use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::Mutex,
};

use brood_graphics::{Color, Command as DrawCommand, Packet, Rect, theme::ashen};
use brood_plugin_sdk::{
    bindings,
    brood::plugin::host,
    exports::brood::plugin::actor::{
        Command, Event, Guest, LifecycleEvent, LocalActorMessage, NotificationEvent,
        PresentationEvent, SessionActorMessage, StartContext, WorkerEvent, WorkerRequest,
    },
    input::{MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_SUPER},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use unicode_width::UnicodeWidthStr;

const WORKER: u64 = 1;
const RUNNER_OUTPUT_TOKEN: u64 = 2;
const CLIENT_INIT: &str = "brood.client-init";
const FILE_ACTOR: &str = "neovim-files";
const RUNNER: &str = "share/brood/neovim-wasm/runner.mjs";

const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TRANSFERS: usize = 4;
const MAX_FILE_STATE_BYTES: usize = MAX_FILE_BYTES * MAX_TRANSFERS;
const MAX_PENDING_OPENS: usize = 32;
const BACKEND_CHUNK_BYTES: usize = 7 * 1024;
const RUNNER_INLINE_BYTES: usize = 8 * 1024;
const RUNNER_OPEN_CHUNK_BYTES: usize = 1024;
const RUNNER_SAVE_CHUNK_BYTES: usize = 7 * 1024;
const MAX_RUNNER_LINE_BYTES: usize = 12 * 1024;
const MAX_RUNNER_BUFFER_BYTES: usize = 24 * 1024;
const MAX_RUNNER_QUEUE_BYTES: usize = MAX_FILE_BYTES + 512 * 1024;
const MAX_RUNNER_WRITE_RECORDS_PER_TURN: usize = 64;
const MAX_RUNNER_NOTICES: usize = 64;
const MAX_SCREEN_BYTES: usize = 2 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_ACTION_BYTES: usize = 16 * 1024;
const MAX_ROWS: usize = 4_096;
const MAX_COLUMNS: u16 = 4_096;
const MIN_RUNNER_ROWS: u16 = 4;
const MAX_RUNNER_ROWS: u16 = 200;
const MIN_RUNNER_COLUMNS: u16 = 20;
const MAX_RUNNER_COLUMNS: u16 = 400;

const FONT_SIZE: f32 = 16.0;
const REQUESTED_LINE_HEIGHT: f32 = 20.0;
const PADDING: f32 = 8.0;
static STATE: Mutex<Option<ClientState>> = Mutex::new(None);

struct NeovimClient;

struct ClientState {
    backend_id: u64,
    session_id: u64,
    instance_id: String,
    pane: u64,
    width: f32,
    height: f32,
    scale: f32,
    cell_width: f32,
    line_height: f32,
    focused: bool,
    input_notification: u64,
    input_generation: u64,
    output_notification: u64,
    output_generation: u64,
    observed_output_generation: u64,
    runner_ready: bool,
    ready_announced: bool,
    files_ready: bool,
    status: String,
    error: Option<String>,
    runner_queue: VecDeque<RunnerInput>,
    runner_queue_bytes: usize,
    runner_uploads: VecDeque<RunnerUpload>,
    notices: VecDeque<RunnerNotice>,
    screen: Option<Screen>,
    layouts: BTreeMap<LayoutKey, u64>,
    next_request: u64,
    next_transfer: u64,
    opens: HashMap<u64, OpenTransfer>,
    saves: HashMap<u64, SaveTransfer>,
    pending_runner_opens: HashMap<String, String>,
    queued_opens: VecDeque<EditorOpen>,
}

impl ClientState {
    fn new(
        backend_id: u64,
        session_id: u64,
        instance_id: String,
        input_notification: u64,
        output_notification: u64,
    ) -> Self {
        Self {
            backend_id,
            session_id,
            instance_id,
            pane: 0,
            width: 640.0,
            height: 480.0,
            scale: 1.0,
            cell_width: 9.0,
            line_height: REQUESTED_LINE_HEIGHT,
            focused: false,
            input_notification,
            input_generation: 0,
            output_notification,
            output_generation: 0,
            observed_output_generation: 0,
            runner_ready: false,
            ready_announced: false,
            files_ready: false,
            status: "Starting local Neovim…".to_string(),
            error: None,
            runner_queue: VecDeque::new(),
            runner_queue_bytes: 0,
            runner_uploads: VecDeque::new(),
            notices: VecDeque::new(),
            screen: None,
            layouts: BTreeMap::new(),
            next_request: 1,
            next_transfer: 1,
            opens: HashMap::new(),
            saves: HashMap::new(),
            pending_runner_opens: HashMap::new(),
            queued_opens: VecDeque::new(),
        }
    }

    fn allocate_request(&mut self) -> u64 {
        let request = self.next_request;
        self.next_request = self.next_request.saturating_add(1).max(1);
        request
    }

    fn allocate_transfer(&mut self) -> u64 {
        let transfer = self.next_transfer;
        self.next_transfer = self.next_transfer.saturating_add(1).max(1);
        transfer
    }

    fn session_message(&self, actor: &str, body: Vec<u8>) -> Command {
        Command::SendSessionMessage(SessionActorMessage {
            backend_id: self.backend_id,
            session_id: self.session_id,
            actor: actor.to_string(),
            body,
        })
    }

    fn file_message(&self, value: Value) -> Option<Command> {
        serde_json::to_vec(&value)
            .ok()
            .map(|body| self.session_message(FILE_ACTOR, body))
    }

    fn init_message(&self, kind: &str) -> Option<Command> {
        serde_json::to_vec(&json!({
            "kind": kind,
            "session_id": self.session_id,
            "actor": self.instance_id,
        }))
        .ok()
        .map(|body| self.session_message("init", body))
    }

    fn init_failure_message(&self, message: &str) -> Option<Command> {
        serde_json::to_vec(&json!({
            "kind": "editor-client-failed",
            "session_id": self.session_id,
            "actor": self.instance_id,
            "message": message.chars().take(2_048).collect::<String>(),
        }))
        .ok()
        .filter(|body| body.len() <= MAX_ACTION_BYTES)
        .map(|body| self.session_message("init", body))
    }

    fn signal_input(&mut self) {
        self.input_generation = self.input_generation.saturating_add(1);
        let _ = host::notification_signal(self.input_notification, self.input_generation);
    }

    fn queue_runner_value(&mut self, value: &Value) -> Result<(), String> {
        let line = encode_runner_line(value)?;
        if self.runner_queue_bytes.saturating_add(line.len()) > MAX_RUNNER_QUEUE_BYTES {
            return Err("runner input queue reached its bounded capacity".to_string());
        }
        self.runner_queue_bytes = self.runner_queue_bytes.saturating_add(line.len());
        self.runner_queue.push_back(RunnerInput::Line(line));
        self.signal_input();
        Ok(())
    }

    fn queue_text(&mut self, message_type: &str, text: &str) -> Result<(), String> {
        let lines = runner_text_records(message_type, text)
            .iter()
            .map(encode_runner_line)
            .collect::<Result<Vec<_>, _>>()?;
        let added = lines.iter().map(Vec::len).sum::<usize>();
        if self.runner_queue_bytes.saturating_add(added) > MAX_RUNNER_QUEUE_BYTES {
            return Err("runner input queue reached its bounded capacity".to_string());
        }
        self.runner_queue_bytes = self.runner_queue_bytes.saturating_add(added);
        self.runner_queue
            .extend(lines.into_iter().map(RunnerInput::Line));
        self.signal_input();
        Ok(())
    }

    fn queue_paste(&mut self, text: String) -> Result<(), String> {
        if text.len() > MAX_FILE_BYTES {
            return Err("paste exceeds the 8 MiB input bound".to_string());
        }
        if self.runner_queue_bytes.saturating_add(text.len()) > MAX_RUNNER_QUEUE_BYTES {
            return Err("runner input queue cannot retain the complete paste".to_string());
        }
        self.runner_queue_bytes = self.runner_queue_bytes.saturating_add(text.len());
        self.runner_queue
            .push_back(RunnerInput::Paste(RunnerPaste { text, offset: 0 }));
        self.signal_input();
        Ok(())
    }

    fn queue_open_upload(&mut self, upload: RunnerUpload) -> Result<(), String> {
        if self.runner_uploads.len() >= MAX_TRANSFERS {
            return Err("runner open queue reached its transfer bound".to_string());
        }
        if !self.can_retain_file_bytes(upload.content.len()) {
            return Err("runner open queue reached its aggregate file-state bound".to_string());
        }
        self.runner_uploads.push_back(upload);
        self.signal_input();
        Ok(())
    }

    fn file_state_bytes(&self) -> usize {
        let open = self
            .opens
            .values()
            .map(|transfer| transfer.content.len())
            .sum::<usize>();
        let save = self
            .saves
            .values()
            .map(|transfer| transfer.content.len())
            .sum::<usize>();
        let upload = self
            .runner_uploads
            .iter()
            .map(|transfer| transfer.content.len())
            .sum::<usize>();
        open.saturating_add(save).saturating_add(upload)
    }

    fn can_retain_file_bytes(&self, additional: usize) -> bool {
        self.file_state_bytes().saturating_add(additional) <= MAX_FILE_STATE_BYTES
    }

    fn grid_size(&self) -> (u16, u16) {
        let rows = ((self.height - PADDING * 2.0) / self.line_height)
            .floor()
            .clamp(f32::from(MIN_RUNNER_ROWS), f32::from(MAX_RUNNER_ROWS))
            as u16;
        let columns = ((self.width - PADDING * 2.0) / self.cell_width)
            .floor()
            .clamp(f32::from(MIN_RUNNER_COLUMNS), f32::from(MAX_RUNNER_COLUMNS))
            as u16;
        (rows, columns)
    }

    fn record_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into().chars().take(4_096).collect());
    }
}

#[derive(Clone)]
struct OpenTransfer {
    runner_request_id: String,
    path: String,
    line: Option<u32>,
    column: Option<u32>,
    revision: Option<String>,
    size: Option<usize>,
    content: Vec<u8>,
}

struct SaveTransfer {
    runner_request_id: String,
    transfer: u64,
    path: String,
    content: Vec<u8>,
    next_offset: usize,
    inflight: usize,
    commit_sent: bool,
}

struct RunnerUpload {
    request_id: String,
    path: String,
    version: Option<String>,
    content: String,
    line: Option<u32>,
    column: Option<u32>,
    phase: UploadPhase,
}

enum UploadPhase {
    Start,
    Chunks(usize),
    End,
}

enum RunnerNotice {
    Action(Value),
    ActorMessage { target: String, body: Value },
    Save(RunnerSave),
    OpenCompleted { path: String },
    OpenFailed { path: String, message: String },
}

enum RunnerInput {
    Line(Vec<u8>),
    Paste(RunnerPaste),
}

struct RunnerPaste {
    text: String,
    offset: usize,
}

struct RunnerSave {
    request_id: String,
    path: String,
    base_version: String,
    content: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Screen {
    revision: u64,
    columns: u16,
    rows: u16,
    lines: Vec<String>,
    cursor_row: u16,
    cursor_column: u16,
    cursor_visible: bool,
    mode: String,
    buffer_path: String,
    modified: bool,
    readonly: bool,
    cmdline: String,
    message: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LayoutKey {
    text: String,
    color: u32,
    max_width_bits: u32,
}

impl Guest for NeovimClient {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(context)) => start(context),
            Event::Lifecycle(LifecycleEvent::Stop(_)) => stop(),
            Event::Presentation(event) => handle_presentation(event),
            Event::Mailbox(message) => handle_mailbox(&message.sender, &message.body),
            Event::Notification(event) => handle_notification(event),
            Event::Worker(WorkerEvent::Failed(failed)) if failed.worker == WORKER => {
                runner_failed(failed.message)
            }
            Event::Worker(WorkerEvent::Finished(finished)) if finished.worker == WORKER => {
                runner_failed(format!(
                    "local Neovim runner stopped: {}",
                    String::from_utf8_lossy(&finished.output)
                ))
            }
            Event::Worker(WorkerEvent::Cancelled(worker)) if worker == WORKER => Vec::new(),
            _ => Vec::new(),
        }
    }

    async fn worker_run(worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        if worker != WORKER {
            return Err(format!("unknown Neovim client worker {worker}"));
        }
        let (input_notification, output_notification) = decode_worker_payload(&payload)
            .ok_or_else(|| "invalid Neovim runner worker payload".to_string())?;
        run_runner(input_notification, output_notification).await
    }
}

fn start(context: StartContext) -> Vec<Command> {
    let Some(backend_id) = context.backend_id else {
        return vec![Command::LifecycleComplete(Err(
            "Neovim client requires a backend association".to_string(),
        ))];
    };
    let Some(session_id) = context.session_id else {
        return vec![Command::LifecycleComplete(Err(
            "Neovim client requires a surface session".to_string(),
        ))];
    };
    let input_notification = match host::notification_create() {
        Ok(notification) => notification,
        Err(error) => {
            return vec![Command::LifecycleComplete(Err(format!(
                "create Neovim input notification: {error:?}"
            )))];
        }
    };
    let output_notification = match host::notification_create() {
        Ok(notification) => notification,
        Err(error) => {
            let _ = host::notification_drop(input_notification);
            return vec![Command::LifecycleComplete(Err(format!(
                "create Neovim output notification: {error:?}"
            )))];
        }
    };
    let state = ClientState::new(
        backend_id,
        session_id,
        context.instance_id,
        input_notification,
        output_notification,
    );
    if let Ok(mut guard) = STATE.lock() {
        *guard = Some(state);
    }
    let _ = host::notification_watch_actor(output_notification, 0, RUNNER_OUTPUT_TOKEN);
    let mut commands = vec![Command::SpawnWorker(WorkerRequest {
        worker: WORKER,
        payload: encode_worker_payload(input_notification, output_notification),
    })];
    commands.push(Command::LifecycleComplete(Ok(())));
    commands
}

fn stop() -> Vec<Command> {
    let stopped = STATE.lock().ok().and_then(|mut guard| {
        guard.as_mut().and_then(|state| {
            let stopped = state.init_message("editor-client-stopped");
            let _ = state.queue_runner_value(&json!({"v": 1, "type": "shutdown"}));
            for handle in std::mem::take(&mut state.layouts).into_values() {
                let _ = host::graphics_drop_resource(state.pane, handle);
            }
            stopped
        })
    });
    let mut commands = vec![Command::CancelWorker(WORKER)];
    commands.extend(stopped);
    commands.push(Command::LifecycleComplete(Ok(())));
    commands
}

fn encode_worker_payload(input_notification: u64, output_notification: u64) -> Vec<u8> {
    let mut payload = Vec::with_capacity(16);
    payload.extend_from_slice(&input_notification.to_le_bytes());
    payload.extend_from_slice(&output_notification.to_le_bytes());
    payload
}

fn decode_worker_payload(payload: &[u8]) -> Option<(u64, u64)> {
    let input = payload
        .get(..8)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map(u64::from_le_bytes)?;
    let output = payload
        .get(8..16)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map(u64::from_le_bytes)?;
    (payload.len() == 16).then_some((input, output))
}

fn handle_presentation(event: PresentationEvent) -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    let mut commands = Vec::new();
    match event {
        PresentationEvent::PaneCreated(created) => {
            state.pane = created.pane;
            state.width = created.width.max(1.0);
            state.height = created.height.max(1.0);
            state.scale = created.scale.max(0.1);
            if let Ok(metrics) =
                host::graphics_layout_text(state.pane, "M", text_options(ashen::TEXT_RGB))
            {
                state.cell_width = metrics.cell_width.max(1.0);
                state.line_height = metrics.line_height.max(1.0);
                let _ = host::graphics_drop_resource(state.pane, metrics.handle);
            }
            let (rows, columns) = state.grid_size();
            if let Err(error) = state.queue_runner_value(
                &json!({"v": 1, "type": "resize", "columns": columns, "rows": rows}),
            ) {
                state.record_error(error);
            }
            commands.push(Command::SendHostMessage(
                format!(r#"{{"type":"focus-pane","pane":{}}}"#, state.pane).into_bytes(),
            ));
            redraw_or_record_error(state);
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            state.width = resized.width.max(1.0);
            state.height = resized.height.max(1.0);
            state.scale = resized.scale.max(0.1);
            let (rows, columns) = state.grid_size();
            if let Err(error) = state.queue_runner_value(
                &json!({"v": 1, "type": "resize", "columns": columns, "rows": rows}),
            ) {
                state.record_error(error);
            }
            redraw_or_record_error(state);
        }
        PresentationEvent::PaneFocused(focused) if focused.pane == state.pane => {
            state.focused = focused.focused;
            if let Err(error) = state
                .queue_runner_value(&json!({"v": 1, "type": "focus", "focused": focused.focused}))
            {
                state.record_error(error);
            }
            redraw_or_record_error(state);
        }
        PresentationEvent::Key(key) if key.pane == state.pane && key.pressed => {
            if let Some(key) = nvim_key(&key.logical_key, key.modifiers)
                && let Err(error) =
                    state.queue_runner_value(&json!({"v": 1, "type": "key", "key": key}))
            {
                state.record_error(error);
                redraw_or_record_error(state);
            }
        }
        PresentationEvent::Text(text) if text.pane == state.pane => {
            let filtered = text
                .text
                .chars()
                .filter(|character| !character.is_control())
                .collect::<String>();
            if !filtered.is_empty()
                && let Err(error) = state.queue_text("text", &filtered)
            {
                state.record_error(error);
                redraw_or_record_error(state);
            }
        }
        PresentationEvent::Paste(paste) if paste.pane == state.pane => {
            if !paste.text.is_empty()
                && let Err(error) = state.queue_paste(paste.text)
            {
                state.record_error(error);
                redraw_or_record_error(state);
            }
        }
        _ => {}
    }
    commands
}

fn handle_mailbox(sender: &str, body: &[u8]) -> Vec<Command> {
    if body.len() > MAX_ACTION_BYTES {
        return Vec::new();
    }
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    if trusted_file_sender(sender, state.backend_id) {
        let commands = serde_json::from_slice::<Value>(body)
            .ok()
            .map_or_else(Vec::new, |value| handle_file_reply(state, &value));
        redraw_or_record_error(state);
        return commands;
    }
    if !trusted_open_sender(sender, state.backend_id) {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    if value.get("kind").and_then(Value::as_str) == Some("focus") {
        if state.pane == 0 {
            return Vec::new();
        }
        state.focused = true;
        let _ = state.queue_runner_value(&json!({"v": 1, "type": "focus", "focused": true}));
        return vec![Command::SendHostMessage(
            format!(r#"{{"type":"focus-pane","pane":{}}}"#, state.pane).into_bytes(),
        )];
    }
    let Some(open) = parse_open_command(&value) else {
        if value
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "editor-open" | "open-file" | "open"))
        {
            let path = value
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            state.record_error("editor open path or coordinates are invalid");
            redraw_or_record_error(state);
            return editor_open_failure(
                state,
                &path.chars().take(MAX_PATH_BYTES).collect::<String>(),
                "path or coordinates are invalid",
            )
            .into_iter()
            .collect();
        }
        return Vec::new();
    };
    queue_or_start_open(state, open)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditorOpen {
    path: String,
    line: Option<u32>,
    column: Option<u32>,
}

fn active_file_transfers(state: &ClientState) -> usize {
    state
        .opens
        .len()
        .saturating_add(state.saves.len())
        .saturating_add(state.pending_runner_opens.len())
}

fn start_open(state: &mut ClientState, open: EditorOpen) -> Vec<Command> {
    let request = state.allocate_request();
    state.opens.insert(
        request,
        OpenTransfer {
            runner_request_id: format!("open-{request}"),
            path: open.path.clone(),
            line: open.line,
            column: open.column,
            revision: None,
            size: None,
            content: Vec::new(),
        },
    );
    state
        .file_message(json!({
            "kind": "open",
            "request": request,
            "path": open.path,
        }))
        .into_iter()
        .collect()
}

fn queue_or_start_open(state: &mut ClientState, open: EditorOpen) -> Vec<Command> {
    if active_file_transfers(state) < MAX_TRANSFERS && state.queued_opens.is_empty() {
        return start_open(state, open);
    }
    if state.queued_opens.len() >= MAX_PENDING_OPENS {
        state.record_error("editor open queue reached its bounded capacity");
        return editor_open_failure(
            state,
            &open.path,
            "editor open queue reached its bounded capacity",
        )
        .into_iter()
        .collect();
    }
    state.status = format!("Queued {}", open.path);
    state.queued_opens.push_back(open);
    Vec::new()
}

fn start_queued_opens(state: &mut ClientState) -> Vec<Command> {
    let mut commands = Vec::new();
    while active_file_transfers(state) < MAX_TRANSFERS {
        let Some(open) = state.queued_opens.pop_front() else {
            break;
        };
        commands.extend(start_open(state, open));
    }
    commands
}

fn parse_open_command(value: &Value) -> Option<EditorOpen> {
    let object = value.as_object()?;
    if !matches!(
        object.get("kind")?.as_str()?,
        "editor-open" | "open-file" | "open"
    ) {
        return None;
    }
    let path = object.get("path")?.as_str()?;
    if !valid_relative_path(path) {
        return None;
    }
    let line = optional_one_based_u32(object.get("line"))?;
    let column = optional_one_based_u32(object.get("column"))?;
    Some(EditorOpen {
        path: path.to_string(),
        line,
        column,
    })
}

fn optional_one_based_u32(value: Option<&Value>) -> Option<Option<u32>> {
    let Some(value) = value else {
        return Some(None);
    };
    if value.is_null() {
        return Some(None);
    }
    let value = u32::try_from(value.as_u64()?).ok()?;
    (value > 0).then_some(Some(value))
}

fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.starts_with('/')
        && !path.ends_with('/')
        && !path.contains('\0')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

fn trusted_file_sender(sender: &str, backend_id: u64) -> bool {
    sender == format!("session:{backend_id}:{FILE_ACTOR}")
}

fn trusted_open_sender(sender: &str, backend_id: u64) -> bool {
    sender == CLIENT_INIT || sender == format!("session:{backend_id}:init")
}

fn handle_file_reply(state: &mut ClientState, value: &Value) -> Vec<Command> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    match object.get("kind").and_then(Value::as_str) {
        Some("hello") => {
            state.status = "Local Neovim is connected to the remote workspace".to_string();
            Vec::new()
        }
        Some("editor-client-authorized") => {
            let valid = object.get("version").and_then(Value::as_u64) == Some(1)
                && object.get("session_id").and_then(Value::as_u64) == Some(state.session_id)
                && object.get("actor").and_then(Value::as_str) == Some(state.instance_id.as_str());
            if !valid || !state.runner_ready {
                state.record_error("remote file service returned an invalid client authorization");
                return Vec::new();
            }
            if state.files_ready {
                return Vec::new();
            }
            state.files_ready = true;
            state.status = "Local Neovim is connected to the remote workspace".to_string();
            state
                .init_message("editor-files-ready")
                .into_iter()
                .collect()
        }
        Some("open" | "read") => handle_read_reply(state, object),
        Some("save-ready") => handle_save_ready(state, object),
        Some("save-chunk-accepted") => handle_save_chunk_accepted(state, object),
        Some("save-complete") => handle_save_complete(state, object),
        Some("error") => handle_file_error(state, object),
        _ => Vec::new(),
    }
}

fn handle_read_reply(state: &mut ClientState, object: &Map<String, Value>) -> Vec<Command> {
    let Some(request) = object.get("request").and_then(Value::as_u64) else {
        return Vec::new();
    };
    let Some(transfer) = state.opens.get(&request) else {
        return Vec::new();
    };
    if object
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|path| path != transfer.path)
    {
        return fail_open(state, request, "backend returned a different file path");
    }
    let Some(offset) = object
        .get("offset")
        .and_then(Value::as_u64)
        .and_then(|offset| usize::try_from(offset).ok())
    else {
        return fail_open(state, request, "backend file reply has no valid offset");
    };
    if offset != transfer.content.len() {
        return fail_open(state, request, "backend file chunks arrived out of order");
    }
    let Some(encoded) = object.get("data_hex").and_then(Value::as_str) else {
        return fail_open(state, request, "backend file reply has no chunk data");
    };
    let Some(chunk) = decode_hex(encoded.as_bytes()) else {
        return fail_open(state, request, "backend file reply contains invalid hex");
    };
    if chunk.len() > BACKEND_CHUNK_BYTES {
        return fail_open(state, request, "backend file chunk exceeds 7 KiB");
    }
    let size = object
        .get("size")
        .and_then(Value::as_u64)
        .and_then(|size| usize::try_from(size).ok());
    if size.is_some_and(|size| size > MAX_FILE_BYTES) || !state.can_retain_file_bytes(chunk.len()) {
        return fail_open(
            state,
            request,
            "remote file exceeds the bounded editor file state",
        );
    }
    let Some(transfer) = state.opens.get_mut(&request) else {
        return Vec::new();
    };
    if let Some(size) = size {
        if transfer.size.is_some_and(|known| known != size) {
            return fail_open(state, request, "backend changed the file size mid-transfer");
        }
        transfer.size = Some(size);
    }
    if let Some(revision) = object.get("revision").and_then(Value::as_str) {
        if transfer
            .revision
            .as_deref()
            .is_some_and(|known| known != revision)
        {
            return fail_open(
                state,
                request,
                "backend changed the file revision mid-transfer",
            );
        }
        transfer.revision = Some(revision.to_string());
    }
    transfer.content.extend_from_slice(&chunk);
    if transfer
        .size
        .is_some_and(|size| transfer.content.len() > size)
    {
        return fail_open(
            state,
            request,
            "backend returned more file data than declared",
        );
    }
    let eof = object.get("eof").and_then(Value::as_bool).unwrap_or(false);
    if !eof {
        let offset = transfer.content.len();
        let path = transfer.path.clone();
        return state
            .file_message(json!({
                "kind": "read",
                "request": request,
                "path": path,
                "offset": offset,
            }))
            .into_iter()
            .collect();
    }
    if transfer
        .size
        .is_some_and(|size| size != transfer.content.len())
    {
        return fail_open(
            state,
            request,
            "backend ended the file transfer before its declared size",
        );
    }
    let Some(mut transfer) = state.opens.remove(&request) else {
        return Vec::new();
    };
    let content = match String::from_utf8(std::mem::take(&mut transfer.content)) {
        Ok(content) => content,
        Err(_) => {
            state.record_error(format!("{} is not a UTF-8 text file", transfer.path));
            let mut commands = queue_open_error(state, &transfer, "file is not valid UTF-8");
            commands.extend(start_queued_opens(state));
            return commands;
        }
    };
    if contains_nul(content.as_bytes()) {
        state.record_error(format!("{} contains NUL bytes", transfer.path));
        let mut commands = queue_open_error(state, &transfer, "file contains NUL bytes");
        commands.extend(start_queued_opens(state));
        return commands;
    }
    let path = transfer.path.clone();
    if let Err(error) = deliver_open_to_runner(state, transfer, content) {
        state.record_error(error.clone());
        let mut commands: Vec<Command> = editor_open_failure(state, &path, &error)
            .into_iter()
            .collect();
        commands.extend(start_queued_opens(state));
        return commands;
    }
    Vec::new()
}

fn fail_open(state: &mut ClientState, request: u64, message: &str) -> Vec<Command> {
    if let Some(transfer) = state.opens.remove(&request) {
        state.record_error(format!("Could not open {}: {message}", transfer.path));
        let mut commands: Vec<Command> = state
            .file_message(json!({
                "kind": "open-abort",
                "request": request,
            }))
            .into_iter()
            .collect();
        commands.extend(queue_open_error(state, &transfer, message));
        commands.extend(start_queued_opens(state));
        return commands;
    }
    Vec::new()
}

fn queue_open_error(
    state: &mut ClientState,
    transfer: &OpenTransfer,
    message: &str,
) -> Vec<Command> {
    editor_open_failure(state, &transfer.path, message)
        .into_iter()
        .collect()
}

fn editor_open_failure(state: &ClientState, path: &str, message: &str) -> Option<Command> {
    serde_json::to_vec(&json!({
        "kind": "editor-open-failed",
        "source": "neovim-client",
        "backend_id": state.backend_id,
        "session_id": state.session_id,
        "path": path,
        "message": message.chars().take(2_048).collect::<String>(),
    }))
    .ok()
    .filter(|body| body.len() <= MAX_ACTION_BYTES)
    .map(|body| {
        Command::SendLocalMessage(LocalActorMessage {
            actor: CLIENT_INIT.to_string(),
            body,
        })
    })
}

fn editor_opened(state: &ClientState, path: &str) -> Option<Command> {
    serde_json::to_vec(&json!({
        "kind": "editor-opened",
        "source": "neovim-client",
        "backend_id": state.backend_id,
        "session_id": state.session_id,
        "path": path,
    }))
    .ok()
    .filter(|body| body.len() <= MAX_ACTION_BYTES)
    .map(|body| {
        Command::SendLocalMessage(LocalActorMessage {
            actor: CLIENT_INIT.to_string(),
            body,
        })
    })
}

fn deliver_open_to_runner(
    state: &mut ClientState,
    transfer: OpenTransfer,
    content: String,
) -> Result<(), String> {
    if state.pending_runner_opens.len() >= MAX_TRANSFERS {
        return Err("runner has too many pending file opens".to_string());
    }
    let request_id = transfer.runner_request_id.clone();
    let path = transfer.path.clone();
    let inline = json!({
        "v": 1,
        "type": "open-content",
        "request_id": transfer.runner_request_id,
        "path": transfer.path,
        "version": transfer.revision,
        "content": content,
        "line": transfer.line,
        "column": transfer.column,
        "readonly": false,
    });
    if content.len() <= RUNNER_INLINE_BYTES
        && serde_json::to_vec(&inline).is_ok_and(|line| line.len() < MAX_RUNNER_LINE_BYTES)
    {
        state.pending_runner_opens.insert(request_id.clone(), path);
        return state.queue_runner_value(&inline).inspect_err(|_| {
            state.pending_runner_opens.remove(&request_id);
        });
    }
    let upload = RunnerUpload {
        request_id: request_id.clone(),
        path: path.clone(),
        version: inline["version"].as_str().map(str::to_string),
        content,
        line: transfer.line,
        column: transfer.column,
        phase: UploadPhase::Start,
    };
    state.pending_runner_opens.insert(request_id.clone(), path);
    state.queue_open_upload(upload).inspect_err(|_| {
        state.pending_runner_opens.remove(&request_id);
    })
}

fn handle_save_ready(state: &mut ClientState, object: &Map<String, Value>) -> Vec<Command> {
    let Some(request) = object.get("request").and_then(Value::as_u64) else {
        return Vec::new();
    };
    let Some(transfer) = state.saves.get(&request) else {
        return Vec::new();
    };
    let valid = object.get("transfer").and_then(Value::as_u64) == Some(transfer.transfer)
        && object.get("path").and_then(Value::as_str) == Some(transfer.path.as_str())
        && object.get("size").and_then(Value::as_u64) == Some(transfer.content.len() as u64)
        && transfer.next_offset == 0
        && transfer.inflight == 0
        && !transfer.commit_sent;
    if !valid {
        return fail_save_reply(
            state,
            request,
            "backend returned an invalid save-ready reply",
        );
    }
    save_next_command(state, request).into_iter().collect()
}

fn handle_save_chunk_accepted(
    state: &mut ClientState,
    object: &Map<String, Value>,
) -> Vec<Command> {
    let Some(request) = object.get("request").and_then(Value::as_u64) else {
        return Vec::new();
    };
    let Some(transfer) = state.saves.get(&request) else {
        return Vec::new();
    };
    let expected_next = transfer.next_offset.saturating_add(transfer.inflight);
    let valid = object.get("transfer").and_then(Value::as_u64) == Some(transfer.transfer)
        && transfer.inflight > 0
        && !transfer.commit_sent
        && object.get("next_offset").and_then(Value::as_u64) == Some(expected_next as u64);
    if !valid {
        return fail_save_reply(
            state,
            request,
            "backend returned an invalid save chunk acknowledgement",
        );
    }
    let Some(transfer) = state.saves.get_mut(&request) else {
        return Vec::new();
    };
    transfer.next_offset = expected_next;
    transfer.inflight = 0;
    save_next_command(state, request).into_iter().collect()
}

fn save_next_command(state: &mut ClientState, request: u64) -> Option<Command> {
    let value = {
        let transfer = state.saves.get_mut(&request)?;
        if transfer.next_offset >= transfer.content.len() {
            if transfer.commit_sent {
                return None;
            }
            transfer.commit_sent = true;
            json!({
                "kind": "save-commit",
                "request": request,
                "transfer": transfer.transfer,
            })
        } else {
            let end = transfer
                .next_offset
                .saturating_add(BACKEND_CHUNK_BYTES)
                .min(transfer.content.len());
            let chunk = &transfer.content[transfer.next_offset..end];
            transfer.inflight = chunk.len();
            json!({
                "kind": "save-chunk",
                "request": request,
                "transfer": transfer.transfer,
                "offset": transfer.next_offset,
                "data_hex": encode_hex(chunk),
            })
        }
    };
    state.file_message(value)
}

fn handle_save_complete(state: &mut ClientState, object: &Map<String, Value>) -> Vec<Command> {
    let Some(request) = object.get("request").and_then(Value::as_u64) else {
        return Vec::new();
    };
    let Some(transfer) = state.saves.get(&request) else {
        return Vec::new();
    };
    if !valid_save_completion(transfer, object) {
        return fail_save_reply(
            state,
            request,
            "backend returned an invalid save completion",
        );
    }
    let revision = object.get("revision").and_then(Value::as_str);
    let Some(transfer) = state.saves.remove(&request) else {
        return Vec::new();
    };
    let version = revision.unwrap_or_default();
    let result = json!({
        "v": 1,
        "type": "save-result",
        "request_id": transfer.runner_request_id,
        "ok": true,
        "version": version,
    });
    if let Err(error) = state.queue_runner_value(&result) {
        state.record_error(error);
    } else {
        state.status = format!("Saved {}", transfer.path);
        state.error = None;
    }
    start_queued_opens(state)
}

fn handle_file_error(state: &mut ClientState, object: &Map<String, Value>) -> Vec<Command> {
    let Some(request) = object.get("request").and_then(Value::as_u64) else {
        return Vec::new();
    };
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("remote file operation failed")
        .chars()
        .take(2_048)
        .collect::<String>();
    if let Some(transfer) = state.opens.remove(&request) {
        state.record_error(format!("Could not open {}: {message}", transfer.path));
        let mut commands = queue_open_error(state, &transfer, &message);
        commands.extend(start_queued_opens(state));
        return commands;
    }
    if let Some(transfer) = state.saves.remove(&request) {
        state.record_error(format!("Could not save {}: {message}", transfer.path));
        let _ = state.queue_runner_value(&json!({
            "v": 1,
            "type": "save-result",
            "request_id": transfer.runner_request_id,
            "ok": false,
            "error": message,
        }));
        return start_queued_opens(state);
    }
    Vec::new()
}

fn start_save(state: &mut ClientState, save: RunnerSave) -> Vec<Command> {
    if save.request_id.is_empty()
        || save.request_id.len() > MAX_REQUEST_ID_BYTES
        || !valid_relative_path(&save.path)
    {
        return queue_save_failure(state, &save.request_id, "invalid save request");
    }
    if contains_nul(&save.content) {
        return queue_save_failure(
            state,
            &save.request_id,
            "remote buffers cannot be saved with NUL bytes",
        );
    }
    if save.content.len() > MAX_FILE_BYTES || !state.can_retain_file_bytes(save.content.len()) {
        return queue_save_failure(
            state,
            &save.request_id,
            "save exceeds the bounded editor file state",
        );
    }
    if active_file_transfers(state) >= MAX_TRANSFERS {
        return queue_save_failure(
            state,
            &save.request_id,
            "too many editor file transfers are active",
        );
    }
    let request = state.allocate_request();
    let transfer_id = state.allocate_transfer();
    let size = save.content.len();
    let path = save.path.clone();
    let base_revision = save.base_version.clone();
    state.saves.insert(
        request,
        SaveTransfer {
            runner_request_id: save.request_id,
            transfer: transfer_id,
            path: save.path,
            content: save.content,
            next_offset: 0,
            inflight: 0,
            commit_sent: false,
        },
    );
    state
        .file_message(json!({
            "kind": "save-begin",
            "request": request,
            "transfer": transfer_id,
            "path": path,
            "base_revision": base_revision,
            "size": size,
        }))
        .into_iter()
        .collect()
}

fn queue_save_failure(state: &mut ClientState, request_id: &str, message: &str) -> Vec<Command> {
    state.record_error(message);
    let _ = state.queue_runner_value(&json!({
        "v": 1,
        "type": "save-result",
        "request_id": request_id,
        "ok": false,
        "error": message,
    }));
    Vec::new()
}

fn fail_save_reply(state: &mut ClientState, request: u64, message: &str) -> Vec<Command> {
    let Some(transfer) = state.saves.remove(&request) else {
        return Vec::new();
    };
    state.record_error(format!("Could not save {}: {message}", transfer.path));
    let _ = state.queue_runner_value(&json!({
        "v": 1,
        "type": "save-result",
        "request_id": transfer.runner_request_id,
        "ok": false,
        "error": message,
    }));
    let mut commands: Vec<Command> = state
        .file_message(json!({
            "kind": "save-abort",
            "request": request,
            "transfer": transfer.transfer,
        }))
        .into_iter()
        .collect();
    commands.extend(start_queued_opens(state));
    commands
}

fn content_revision(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

fn contains_nul(content: &[u8]) -> bool {
    content.contains(&0)
}

fn valid_save_completion(transfer: &SaveTransfer, object: &Map<String, Value>) -> bool {
    let expected_revision = content_revision(&transfer.content);
    object.get("transfer").and_then(Value::as_u64) == Some(transfer.transfer)
        && object.get("path").and_then(Value::as_str) == Some(transfer.path.as_str())
        && object.get("size").and_then(Value::as_u64) == Some(transfer.content.len() as u64)
        && object.get("revision").and_then(Value::as_str) == Some(expected_revision.as_str())
        && transfer.next_offset == transfer.content.len()
        && transfer.inflight == 0
        && transfer.commit_sent
}

fn handle_notification(event: NotificationEvent) -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    if event.token != RUNNER_OUTPUT_TOKEN || event.notification != state.output_notification {
        return Vec::new();
    }
    state.observed_output_generation = state.observed_output_generation.max(event.generation);
    let _ = host::notification_watch_actor(
        state.output_notification,
        state.observed_output_generation,
        RUNNER_OUTPUT_TOKEN,
    );
    let notices = std::mem::take(&mut state.notices);
    let mut commands = Vec::new();
    if state.runner_ready && !state.ready_announced {
        state.ready_announced = true;
        commands.extend(state.init_message("editor-client-ready"));
    }
    for notice in notices {
        match notice {
            RunnerNotice::Action(action) => {
                commands.extend(route_runner_action(state, &action));
            }
            RunnerNotice::ActorMessage { target, body } => {
                commands.extend(route_actor_message(state, &target, body));
            }
            RunnerNotice::Save(save) => commands.extend(start_save(state, save)),
            RunnerNotice::OpenCompleted { path } => {
                commands.extend(editor_opened(state, &path));
            }
            RunnerNotice::OpenFailed { path, message } => {
                commands.extend(editor_open_failure(state, &path, &message));
            }
        }
    }
    commands.extend(start_queued_opens(state));
    redraw_or_record_error(state);
    commands
}

fn route_runner_action(state: &ClientState, value: &Value) -> Vec<Command> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let Some(action) = object.get("action").and_then(Value::as_str) else {
        return Vec::new();
    };
    if !matches!(action, "diff" | "terminal" | "agent" | "generic") {
        return Vec::new();
    }
    let mut envelope = Map::new();
    envelope.insert(
        "kind".to_string(),
        Value::String("editor-action".to_string()),
    );
    envelope.insert("action".to_string(), Value::String(action.to_string()));
    envelope.insert(
        "source".to_string(),
        Value::String("neovim-client".to_string()),
    );
    envelope.insert("backend_id".to_string(), Value::from(state.backend_id));
    envelope.insert("session_id".to_string(), Value::from(state.session_id));
    copy_bounded_fields(
        object,
        &mut envelope,
        &["command", "prompt", "agent", "name", "body", "payload"],
    );
    local_editor_action(envelope).into_iter().collect()
}

fn route_actor_message(state: &ClientState, target: &str, body: Value) -> Vec<Command> {
    if target.len() > 128 {
        return Vec::new();
    }
    let mut envelope = Map::new();
    envelope.insert(
        "kind".to_string(),
        Value::String("editor-action".to_string()),
    );
    envelope.insert("action".to_string(), Value::String("generic".to_string()));
    envelope.insert(
        "source".to_string(),
        Value::String("neovim-client".to_string()),
    );
    envelope.insert("backend_id".to_string(), Value::from(state.backend_id));
    envelope.insert("session_id".to_string(), Value::from(state.session_id));
    envelope.insert("target".to_string(), Value::String(target.to_string()));
    if let Value::Object(object) = &body
        && object.get("kind").and_then(Value::as_str) == Some("editor-action")
    {
        if let Some(action) = object.get("action").and_then(Value::as_str)
            && matches!(action, "diff" | "terminal" | "agent" | "generic")
        {
            envelope.insert("action".to_string(), Value::String(action.to_string()));
        }
        copy_bounded_fields(
            object,
            &mut envelope,
            &["command", "prompt", "agent", "name", "body", "payload"],
        );
    } else if serde_json::to_vec(&body).is_ok_and(|bytes| bytes.len() <= 8 * 1024) {
        envelope.insert("body".to_string(), body);
    }
    local_editor_action(envelope).into_iter().collect()
}

fn copy_bounded_fields(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    fields: &[&str],
) {
    for field in fields {
        let Some(value) = source.get(*field) else {
            continue;
        };
        if serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= 8 * 1024) {
            target.insert((*field).to_string(), value.clone());
        }
    }
}

fn local_editor_action(envelope: Map<String, Value>) -> Option<Command> {
    serde_json::to_vec(&Value::Object(envelope))
        .ok()
        .filter(|body| body.len() <= MAX_ACTION_BYTES)
        .map(|body| {
            Command::SendLocalMessage(LocalActorMessage {
                actor: CLIENT_INIT.to_string(),
                body,
            })
        })
}

fn runner_failed(message: String) -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    let bounded_message = message.chars().take(2_048).collect::<String>();
    state.runner_ready = false;
    state.ready_announced = false;
    state.files_ready = false;
    state.record_error(bounded_message.clone());
    redraw_or_record_error(state);
    state
        .init_failure_message(&bounded_message)
        .into_iter()
        .collect()
}

fn redraw_or_record_error(state: &mut ClientState) {
    if state.pane == 0 {
        return;
    }
    if let Err(error) = redraw(state) {
        state.record_error(error);
    }
}

fn redraw(state: &mut ClientState) -> Result<(), String> {
    let mut layouts = std::mem::take(&mut state.layouts);
    let result = redraw_with_layouts(state, &mut layouts);
    state.layouts = layouts;
    result
}

fn redraw_with_layouts(
    state: &ClientState,
    layouts: &mut BTreeMap<LayoutKey, u64>,
) -> Result<(), String> {
    let mut packet = Packet::new();
    let mut used = BTreeSet::new();
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, state.height),
        color: ashen::BACKGROUND,
    });
    packet.push(DrawCommand::Save);
    packet.push(DrawCommand::ClipRect(Rect::new(
        0.0,
        0.0,
        state.width,
        state.height,
    )));
    if let Some(screen) = &state.screen {
        let overlay = if screen.cmdline.is_empty() {
            &screen.message
        } else {
            &screen.cmdline
        };
        let overlay_row = usize::from(screen.rows.saturating_sub(1));
        let overlay_y = PADDING + overlay_row as f32 * state.line_height;
        if !overlay.is_empty() {
            packet.push(DrawCommand::FillRect {
                rect: Rect::new(0.0, overlay_y, state.width, state.line_height),
                color: ashen::SURFACE,
            });
        }
        if screen.cursor_visible {
            let x = PADDING + f32::from(screen.cursor_column) * state.cell_width;
            let y = PADDING + f32::from(screen.cursor_row) * state.line_height;
            packet.push(DrawCommand::FillRect {
                rect: Rect::new(x, y, state.cell_width.max(1.0), state.line_height.max(1.0)),
                color: if state.focused {
                    Color::rgba(ashen::ACCENT.r, ashen::ACCENT.g, ashen::ACCENT.b, 0.58)
                } else {
                    ashen::BORDER
                },
            });
        }
        let visible_rows = ((state.height - PADDING) / state.line_height)
            .ceil()
            .max(1.0) as usize;
        for (row, text) in screen.lines.iter().take(visible_rows).enumerate() {
            if text.is_empty() || (!overlay.is_empty() && row == overlay_row) {
                continue;
            }
            add_text(
                state.pane,
                &mut packet,
                layouts,
                &mut used,
                text,
                ashen::TEXT_RGB,
                PADDING,
                PADDING + row as f32 * state.line_height,
                (state.width - PADDING * 2.0).max(state.cell_width),
            )?;
        }
        if !overlay.is_empty() {
            add_text(
                state.pane,
                &mut packet,
                layouts,
                &mut used,
                overlay,
                0x00e5_e5e5,
                PADDING,
                overlay_y,
                (state.width - PADDING * 2.0).max(state.cell_width),
            )?;
        }
    } else {
        let heading = state.error.as_deref().unwrap_or(if state.runner_ready {
            "Waiting for Neovim's first screen…"
        } else {
            "Starting local Neovim…"
        });
        add_text(
            state.pane,
            &mut packet,
            layouts,
            &mut used,
            heading,
            if state.error.is_some() {
                ashen::ERROR_RGB
            } else {
                ashen::TEXT_RGB
            },
            20.0,
            20.0,
            (state.width - 40.0).max(40.0),
        )?;
        if state.error.is_none() {
            add_text(
                state.pane,
                &mut packet,
                layouts,
                &mut used,
                &state.status,
                0x0073_7373,
                20.0,
                50.0,
                (state.width - 40.0).max(40.0),
            )?;
        }
    }
    packet.push(DrawCommand::Restore);
    if let Some(error) = &state.error
        && state.screen.is_some()
    {
        let height = state.line_height + 8.0;
        packet.push(DrawCommand::FillRect {
            rect: Rect::new(0.0, (state.height - height).max(0.0), state.width, height),
            color: ashen::DANGER,
        });
        add_text(
            state.pane,
            &mut packet,
            layouts,
            &mut used,
            &one_line(error, 240),
            0x00e5_e5e5,
            PADDING,
            (state.height - height + 4.0).max(0.0),
            (state.width - PADDING * 2.0).max(40.0),
        )?;
    }
    let bytes = packet.encode().map_err(|error| error.to_string())?;
    host::graphics_submit_draw_packet(state.pane, &bytes)
        .map_err(|error| format!("submit Neovim screen: {error:?}"))?;

    let stale = layouts
        .iter()
        .filter_map(|(key, handle)| (!used.contains(handle)).then_some(key.clone()))
        .collect::<Vec<_>>();
    for key in stale {
        if let Some(handle) = layouts.remove(&key) {
            let _ = host::graphics_drop_resource(state.pane, handle);
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "text, style, geometry, and resource tracking define one draw"
)]
fn add_text(
    pane: u64,
    packet: &mut Packet,
    layouts: &mut BTreeMap<LayoutKey, u64>,
    used: &mut BTreeSet<u64>,
    text: &str,
    color: u32,
    x: f32,
    y: f32,
    max_width: f32,
) -> Result<(), String> {
    let key = LayoutKey {
        text: text.to_string(),
        color,
        max_width_bits: max_width.to_bits(),
    };
    let handle = if let Some(handle) = layouts.get(&key) {
        *handle
    } else {
        let layout =
            host::graphics_layout_text(pane, text, text_options_with_width(color, max_width))
                .map_err(|error| format!("layout Neovim text: {error:?}"))?;
        layouts.insert(key, layout.handle);
        layout.handle
    };
    used.insert(handle);
    packet.push(DrawCommand::DrawTextLayout {
        layout: handle,
        x,
        y,
    });
    Ok(())
}

fn text_options(color: u32) -> host::TextLayoutOptions {
    text_options_with_width(color, 64.0)
}

fn text_options_with_width(color: u32, max_width: f32) -> host::TextLayoutOptions {
    let [red, green, blue] = rgb_channels(color);
    host::TextLayoutOptions {
        font_intent: host::FontIntent::Monospace,
        font_weight: host::FontWeight::Normal,
        font_style: host::FontStyle::Normal,
        font_size: FONT_SIZE,
        max_width: max_width.max(1.0),
        line_height: REQUESTED_LINE_HEIGHT,
        red,
        green,
        blue,
        alpha: 1.0,
    }
}

fn rgb_channels(rgb: u32) -> [f32; 3] {
    [
        ((rgb >> 16) & 0xff) as f32 / 255.0,
        ((rgb >> 8) & 0xff) as f32 / 255.0,
        (rgb & 0xff) as f32 / 255.0,
    ]
}

fn one_line(text: &str, limit: usize) -> String {
    text.replace(['\n', '\r'], " ")
        .chars()
        .take(limit)
        .collect()
}

fn parse_screen(value: &Value) -> Result<Screen, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "screen record is not an object".to_string())?;
    let lines_value = object
        .get("lines")
        .or_else(|| object.get("grid"))
        .and_then(Value::as_array)
        .ok_or_else(|| "screen record has no lines".to_string())?;
    if lines_value.len() > MAX_ROWS {
        return Err("screen row count exceeds the bound".to_string());
    }
    let mut bytes = 0_usize;
    let mut lines = Vec::with_capacity(lines_value.len());
    for line in lines_value {
        let text = match line {
            Value::String(text) => text.clone(),
            Value::Object(object) => object
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => return Err("screen line is not text".to_string()),
        };
        bytes = bytes.saturating_add(text.len());
        if bytes > MAX_SCREEN_BYTES {
            return Err("screen text exceeds the state bound".to_string());
        }
        lines.push(text);
    }
    let rows = object
        .get("rows")
        .and_then(Value::as_u64)
        .and_then(|rows| u16::try_from(rows).ok())
        .unwrap_or_else(|| u16::try_from(lines.len()).unwrap_or(u16::MAX));
    if rows == 0 || usize::from(rows) > MAX_ROWS {
        return Err("screen rows are invalid".to_string());
    }
    let inferred_columns = lines
        .iter()
        .map(|line| UnicodeWidthStr::width(line.as_str()))
        .max()
        .unwrap_or(1)
        .clamp(1, usize::from(MAX_COLUMNS)) as u16;
    let columns = object
        .get("columns")
        .or_else(|| object.get("cols"))
        .and_then(Value::as_u64)
        .and_then(|columns| u16::try_from(columns).ok())
        .unwrap_or(inferred_columns);
    if columns == 0 || columns > MAX_COLUMNS {
        return Err("screen columns are invalid".to_string());
    }
    let cursor = object.get("cursor").and_then(Value::as_object);
    let cursor_row = cursor
        .and_then(|cursor| cursor.get("row"))
        .and_then(Value::as_u64)
        .and_then(|row| u16::try_from(row).ok())
        .unwrap_or(0)
        .min(rows.saturating_sub(1));
    let cursor_column = cursor
        .and_then(|cursor| cursor.get("column").or_else(|| cursor.get("col")))
        .and_then(Value::as_u64)
        .and_then(|column| u16::try_from(column).ok())
        .unwrap_or(0)
        .min(columns.saturating_sub(1));
    let cursor_visible = cursor
        .and_then(|cursor| cursor.get("visible"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let buffer = object.get("buffer").and_then(Value::as_object);
    Ok(Screen {
        revision: object.get("revision").and_then(Value::as_u64).unwrap_or(0),
        columns,
        rows,
        lines,
        cursor_row,
        cursor_column,
        cursor_visible,
        mode: object
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(64)
            .collect(),
        buffer_path: buffer
            .and_then(|buffer| buffer.get("path"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(MAX_PATH_BYTES)
            .collect(),
        modified: buffer
            .and_then(|buffer| buffer.get("modified"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        readonly: buffer
            .and_then(|buffer| buffer.get("readonly"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cmdline: parse_cmdline(object.get("cmdline")),
        message: object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(2_048)
            .collect(),
    })
}

fn parse_cmdline(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(cmdline)) => cmdline.chars().take(2_048).collect(),
        Some(Value::Object(cmdline)) => {
            let kind = cmdline
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let text = cmdline
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("{kind}{text}").chars().take(2_048).collect()
        }
        _ => String::new(),
    }
}

struct RunnerParser {
    buffer: Vec<u8>,
    saves: HashMap<String, RunnerSaveAssembly>,
    save_bytes: usize,
}

struct RunnerSaveAssembly {
    path: String,
    base_version: String,
    declared_size: usize,
    content: Vec<u8>,
}

impl RunnerParser {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            saves: HashMap::new(),
            save_bytes: 0,
        }
    }

    fn push(&mut self, bytes: &[u8], output_notification: u64) -> Result<(), String> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_RUNNER_BUFFER_BYTES {
            self.buffer.clear();
            return Err("Neovim runner produced an overlong JSONL record".to_string());
        }
        self.buffer.extend_from_slice(bytes);
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_RUNNER_LINE_BYTES {
                return Err("Neovim runner JSONL record exceeds 12 KiB".to_string());
            }
            let value = serde_json::from_slice::<Value>(&line)
                .map_err(|error| format!("parse Neovim runner JSONL: {error}"))?;
            self.record(value, output_notification)?;
        }
        Ok(())
    }

    fn finish(&mut self, output_notification: u64) -> Result<(), String> {
        if self.buffer.iter().any(|byte| !byte.is_ascii_whitespace()) {
            let line = std::mem::take(&mut self.buffer);
            if line.len() > MAX_RUNNER_LINE_BYTES {
                return Err("Neovim runner final JSON record exceeds 12 KiB".to_string());
            }
            let value = serde_json::from_slice::<Value>(&line)
                .map_err(|error| format!("parse final Neovim runner JSON: {error}"))?;
            self.record(value, output_notification)?;
        }
        Ok(())
    }

    fn record(&mut self, value: Value, output_notification: u64) -> Result<(), String> {
        let object = value
            .as_object()
            .ok_or_else(|| "Neovim runner record is not an object".to_string())?;
        if object
            .get("v")
            .and_then(Value::as_u64)
            .is_some_and(|version| version != 1)
        {
            return Ok(());
        }
        match object.get("type").and_then(Value::as_str) {
            Some("ready") => publish_runner_update(output_notification, |state| {
                state.runner_ready = true;
                state.status = object
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Local Neovim is ready")
                    .chars()
                    .take(2_048)
                    .collect();
                state.error = None;
            }),
            Some("status") => publish_runner_update(output_notification, |state| {
                state.status = object
                    .get("message")
                    .or_else(|| object.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .chars()
                    .take(2_048)
                    .collect();
            }),
            Some("error") => publish_runner_error(output_notification, object),
            Some("screen") => {
                let screen = parse_screen(&value)?;
                publish_runner_update(output_notification, move |state| {
                    if state
                        .screen
                        .as_ref()
                        .is_none_or(|current| screen.revision >= current.revision)
                    {
                        state.status = format!(
                            "{} {}{}{} · {}×{}",
                            screen.mode,
                            screen.buffer_path,
                            if screen.modified { " [+]" } else { "" },
                            if screen.readonly { " [readonly]" } else { "" },
                            screen.columns,
                            screen.rows,
                        )
                        .trim()
                        .to_string();
                        state.screen = Some(screen);
                    }
                });
            }
            Some("action") => {
                publish_notice(output_notification, RunnerNotice::Action(value));
            }
            Some("actor-message") => {
                let target = object
                    .get("target")
                    .and_then(Value::as_str)
                    .unwrap_or(CLIENT_INIT);
                if target.len() <= 128 {
                    let body = object.get("body").cloned().unwrap_or(Value::Null);
                    publish_notice(
                        output_notification,
                        RunnerNotice::ActorMessage {
                            target: target.to_string(),
                            body,
                        },
                    );
                }
            }
            Some("save-request") => {
                let save = parse_inline_save(object)?;
                publish_runner_save(output_notification, save);
            }
            Some("save-request-start") => self.start_save(object)?,
            Some("save-request-chunk") => self.save_chunk(object)?,
            Some("save-request-end") => {
                let save = self.end_save(object)?;
                publish_runner_save(output_notification, save);
            }
            Some("open-result") => {
                let request_id = object
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                publish_runner_opened(output_notification, &request_id);
            }
            _ => {}
        }
        Ok(())
    }

    fn start_save(&mut self, object: &Map<String, Value>) -> Result<(), String> {
        if self.saves.len() >= MAX_TRANSFERS {
            return Err("Neovim runner has too many save transfers".to_string());
        }
        let request_id = bounded_request_id(object)?;
        if self.saves.contains_key(request_id) {
            return Err("Neovim runner reused an active save request ID".to_string());
        }
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| valid_relative_path(path))
            .ok_or_else(|| "Neovim runner save path is invalid".to_string())?;
        let declared_size = object
            .get("size")
            .and_then(Value::as_u64)
            .and_then(|size| usize::try_from(size).ok())
            .filter(|size| *size <= MAX_FILE_BYTES)
            .ok_or_else(|| "Neovim runner save size is invalid".to_string())?;
        if self.save_bytes.saturating_add(declared_size) > MAX_FILE_STATE_BYTES {
            return Err("Neovim runner saves exceed the aggregate assembly bound".to_string());
        }
        self.save_bytes = self.save_bytes.saturating_add(declared_size);
        self.saves.insert(
            request_id.to_string(),
            RunnerSaveAssembly {
                path: path.to_string(),
                base_version: object
                    .get("base_version")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                declared_size,
                content: Vec::with_capacity(declared_size),
            },
        );
        Ok(())
    }

    fn save_chunk(&mut self, object: &Map<String, Value>) -> Result<(), String> {
        let request_id = bounded_request_id(object)?;
        let transfer = self
            .saves
            .get_mut(request_id)
            .ok_or_else(|| "Neovim runner save chunk has no active transfer".to_string())?;
        let offset = object
            .get("offset")
            .and_then(Value::as_u64)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| "Neovim runner save chunk offset is invalid".to_string())?;
        if offset != transfer.content.len() {
            return Err("Neovim runner save chunks are out of order".to_string());
        }
        let content = object
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| "Neovim runner save chunk has no UTF-8 content".to_string())?;
        if content.len() > RUNNER_SAVE_CHUNK_BYTES {
            return Err("Neovim runner save chunk exceeds its bound".to_string());
        }
        if transfer.content.len().saturating_add(content.len()) > transfer.declared_size {
            return Err("Neovim runner save exceeds its declared size".to_string());
        }
        transfer.content.extend_from_slice(content.as_bytes());
        Ok(())
    }

    fn end_save(&mut self, object: &Map<String, Value>) -> Result<RunnerSave, String> {
        let request_id = bounded_request_id(object)?;
        let transfer = self
            .saves
            .remove(request_id)
            .ok_or_else(|| "Neovim runner ended an unknown save transfer".to_string())?;
        self.save_bytes = self.save_bytes.saturating_sub(transfer.declared_size);
        if transfer.content.len() != transfer.declared_size {
            return Err("Neovim runner save ended before its declared size".to_string());
        }
        Ok(RunnerSave {
            request_id: request_id.to_string(),
            path: transfer.path,
            base_version: transfer.base_version,
            content: transfer.content,
        })
    }
}

fn bounded_request_id(object: &Map<String, Value>) -> Result<&str, String> {
    object
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|request| !request.is_empty() && request.len() <= MAX_REQUEST_ID_BYTES)
        .ok_or_else(|| "Neovim runner request ID is invalid".to_string())
}

fn parse_inline_save(object: &Map<String, Value>) -> Result<RunnerSave, String> {
    let request_id = bounded_request_id(object)?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| valid_relative_path(path))
        .ok_or_else(|| "Neovim runner save path is invalid".to_string())?;
    let content = object
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "Neovim runner save has no UTF-8 content".to_string())?;
    if content.len() > MAX_FILE_BYTES {
        return Err("Neovim runner save exceeds 8 MiB".to_string());
    }
    Ok(RunnerSave {
        request_id: request_id.to_string(),
        path: path.to_string(),
        base_version: object
            .get("base_version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        content: content.as_bytes().to_vec(),
    })
}

fn publish_runner_save(output_notification: u64, save: RunnerSave) {
    let Ok(mut guard) = STATE.lock() else {
        return;
    };
    let Some(state) = guard.as_mut() else {
        return;
    };
    if !state.can_retain_file_bytes(save.content.len()) {
        let _ = state.queue_runner_value(&json!({
            "v": 1,
            "type": "save-result",
            "request_id": save.request_id,
            "ok": false,
            "error": "save would exceed the aggregate editor file-state bound",
        }));
        return;
    }
    let request_id = save.request_id.clone();
    if !push_critical_notice(state, RunnerNotice::Save(save)) {
        let _ = state.queue_runner_value(&json!({
            "v": 1,
            "type": "save-result",
            "request_id": request_id,
            "ok": false,
            "error": "Neovim runner notice queue reached its bound",
        }));
    }
    signal_output(state, output_notification);
}

fn publish_runner_opened(output_notification: u64, request_id: &str) {
    let Ok(mut guard) = STATE.lock() else {
        return;
    };
    let Some(state) = guard.as_mut() else {
        return;
    };
    if let Some(path) = state.pending_runner_opens.remove(request_id) {
        state.status = format!("Opened {path}");
        state.error = None;
        let _ = push_critical_notice(state, RunnerNotice::OpenCompleted { path });
    }
    signal_output(state, output_notification);
}

fn publish_runner_error(output_notification: u64, object: &Map<String, Value>) {
    let message = object
        .get("message")
        .or_else(|| object.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("local Neovim runner reported an error")
        .chars()
        .take(2_048)
        .collect::<String>();
    let request_id = object
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Ok(mut guard) = STATE.lock() else {
        return;
    };
    let Some(state) = guard.as_mut() else {
        return;
    };
    state.record_error(message.clone());
    if let Some(path) = request_id
        .as_deref()
        .and_then(|request| state.pending_runner_opens.remove(request))
    {
        let _ = push_critical_notice(state, RunnerNotice::OpenFailed { path, message });
    }
    signal_output(state, output_notification);
}

fn publish_notice(output_notification: u64, notice: RunnerNotice) {
    let Ok(mut guard) = STATE.lock() else {
        return;
    };
    let Some(state) = guard.as_mut() else {
        return;
    };
    if state.notices.len() >= MAX_RUNNER_NOTICES {
        state.record_error("Neovim runner notice queue reached its bound");
    } else {
        state.notices.push_back(notice);
    }
    signal_output(state, output_notification);
}

fn push_critical_notice(state: &mut ClientState, notice: RunnerNotice) -> bool {
    if state.notices.len() >= MAX_RUNNER_NOTICES {
        if let Some(index) = state.notices.iter().position(|queued| {
            matches!(
                queued,
                RunnerNotice::Action(_) | RunnerNotice::ActorMessage { .. }
            )
        }) {
            state.notices.remove(index);
        } else {
            state.record_error("Neovim runner notice queue reached its bound");
            return false;
        }
    }
    state.notices.push_back(notice);
    true
}

fn publish_runner_update(output_notification: u64, update: impl FnOnce(&mut ClientState)) {
    let Ok(mut guard) = STATE.lock() else {
        return;
    };
    let Some(state) = guard.as_mut() else {
        return;
    };
    update(state);
    signal_output(state, output_notification);
}

fn signal_output(state: &mut ClientState, output_notification: u64) {
    if state.output_notification != output_notification {
        return;
    }
    state.output_generation = state.output_generation.saturating_add(1);
    let _ = host::notification_signal(output_notification, state.output_generation);
}

async fn run_runner(input_notification: u64, output_notification: u64) -> Result<Vec<u8>, String> {
    let process = host::subprocess_spawn("node", &[RUNNER.to_string()])
        .map_err(|error| format!("spawn local Neovim runner: {error:?}"))?;
    let result = run_attached_runner(&process, input_notification, output_notification).await;
    cleanup_process(process, result).await
}

async fn run_attached_runner(
    process: &host::ProcessIo,
    input_notification: u64,
    output_notification: u64,
) -> Result<Vec<u8>, String> {
    let stdout_notification = host::stream_notification(process.stdout)
        .map_err(|error| format!("Neovim runner stdout notification: {error:?}"))?;
    let stderr_notification = host::stream_notification(process.stderr)
        .map_err(|error| format!("Neovim runner stderr notification: {error:?}"))?;
    let stdin_notification = host::stream_notification(process.stdin)
        .map_err(|error| format!("Neovim runner stdin notification: {error:?}"))?;
    let child_notification = host::child_notification(process.child)
        .map_err(|error| format!("Neovim runner child notification: {error:?}"))?;
    let mut parser = RunnerParser::new();
    let mut stderr = Vec::new();
    let mut pending = Vec::new();
    let mut pending_offset = 0_usize;
    let mut input_generation = 0_u64;
    let mut stdout_generation = 0_u64;
    let mut stderr_generation = 0_u64;
    let mut stdin_generation = 0_u64;
    let mut child_generation = 0_u64;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut bytes_read = 0_u64;
    let exit = loop {
        let mut stdin_blocked = false;
        let mut input_batch_exhausted = false;
        let mut input_records = 0_usize;
        loop {
            if pending_offset >= pending.len() {
                if input_records >= MAX_RUNNER_WRITE_RECORDS_PER_TURN {
                    input_batch_exhausted = true;
                    break;
                }
                pending.clear();
                pending_offset = 0;
                let Some(line) = take_runner_input_line()? else {
                    break;
                };
                pending = line;
                input_records = input_records.saturating_add(1);
            }
            while pending_offset < pending.len() {
                match host::stream_write(process.stdin, &pending[pending_offset..]) {
                    Ok(written) if written > 0 => {
                        pending_offset = pending_offset.saturating_add(written as usize);
                    }
                    Ok(_) | Err(host::RuntimeIoError::WouldBlock) => {
                        stdin_blocked = true;
                        break;
                    }
                    Err(error) => return Err(format!("write Neovim runner stdin: {error:?}")),
                }
            }
            if stdin_blocked {
                break;
            }
        }
        if !stdout_eof {
            stdout_eof = drain_runner_stdout(
                process.stdout,
                &mut parser,
                output_notification,
                &mut bytes_read,
            )?;
        }
        if !stderr_eof {
            stderr_eof = drain_bounded_stderr(process.stderr, &mut stderr)?;
        }
        let status = host::child_status(process.child)
            .map_err(|error| format!("Neovim runner child status: {error:?}"))?;
        if status.0 != host::ChildState::Running {
            if !stdout_eof {
                let _ = drain_runner_stdout(
                    process.stdout,
                    &mut parser,
                    output_notification,
                    &mut bytes_read,
                );
            }
            let _ = parser.finish(output_notification);
            break status;
        }
        if input_batch_exhausted {
            if host::worker_yield().await {
                return Ok(b"{\"cancelled\":true}".to_vec());
            }
            continue;
        }

        let mut pollables = Vec::<u64>::new();
        let mut kinds = Vec::<WaitKind>::new();
        add_wait(
            &mut pollables,
            &mut kinds,
            input_notification,
            input_generation,
            WaitKind::Input,
        )?;
        if !stdout_eof {
            add_wait(
                &mut pollables,
                &mut kinds,
                stdout_notification,
                stdout_generation,
                WaitKind::Stdout,
            )?;
        }
        if !stderr_eof {
            add_wait(
                &mut pollables,
                &mut kinds,
                stderr_notification,
                stderr_generation,
                WaitKind::Stderr,
            )?;
        }
        if stdin_blocked {
            add_wait(
                &mut pollables,
                &mut kinds,
                stdin_notification,
                stdin_generation,
                WaitKind::Stdin,
            )?;
        }
        add_wait(
            &mut pollables,
            &mut kinds,
            child_notification,
            child_generation,
            WaitKind::Child,
        )?;
        let ready = host::worker_wait_any(pollables.clone(), None, host::WaitMode::Fair).await;
        for pollable in pollables {
            let _ = host::pollable_drop(pollable);
        }
        match ready {
            Ok(ready) => match kinds.get(ready.index as usize) {
                Some(WaitKind::Input) => input_generation = ready.generation,
                Some(WaitKind::Stdout) => stdout_generation = ready.generation,
                Some(WaitKind::Stderr) => stderr_generation = ready.generation,
                Some(WaitKind::Stdin) => stdin_generation = ready.generation,
                Some(WaitKind::Child) => child_generation = ready.generation,
                None => {}
            },
            Err(host::WaitError::Cancelled) => {
                return Ok(b"{\"cancelled\":true}".to_vec());
            }
            Err(error) => {
                return Err(format!("wait for Neovim runner I/O: {error:?}"));
            }
        }
    };
    if !stderr.is_empty() && exit.1 != Some(0) {
        publish_runner_update(output_notification, |state| {
            state.record_error(format!(
                "local Neovim runner failed: {}",
                one_line(&String::from_utf8_lossy(&stderr), 2_048)
            ));
        });
    }
    let output = format!(
        "{{\"bytes_read\":{bytes_read},\"state\":\"{:?}\",\"code\":{}}}",
        exit.0,
        exit.1
            .map_or_else(|| "null".to_string(), |code| code.to_string())
    )
    .into_bytes();
    Ok(output)
}

#[derive(Clone, Copy)]
enum WaitKind {
    Input,
    Stdout,
    Stderr,
    Stdin,
    Child,
}

fn add_wait(
    pollables: &mut Vec<u64>,
    kinds: &mut Vec<WaitKind>,
    notification: u64,
    generation: u64,
    kind: WaitKind,
) -> Result<(), String> {
    let pollable = match host::notification_pollable(notification, generation) {
        Ok(pollable) => pollable,
        Err(error) => {
            for pollable in pollables.drain(..) {
                let _ = host::pollable_drop(pollable);
            }
            kinds.clear();
            return Err(format!("create Neovim runner pollable: {error:?}"));
        }
    };
    pollables.push(pollable);
    kinds.push(kind);
    Ok(())
}

async fn cleanup_process(
    process: host::ProcessIo,
    result: Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    let _ = host::stream_close_write(process.stdin);
    let _ = host::stream_drop(process.stdin).await;
    let _ = host::stream_drop(process.stdout).await;
    let _ = host::stream_drop(process.stderr).await;
    let _ = host::child_drop(process.child).await;
    result
}

fn drain_runner_stdout(
    stream: u64,
    parser: &mut RunnerParser,
    output_notification: u64,
    bytes_read: &mut u64,
) -> Result<bool, String> {
    loop {
        match host::stream_read(stream, 16 * 1024)
            .map_err(|error| format!("read Neovim runner stdout: {error:?}"))?
        {
            host::StreamReadResult::Data(bytes) => {
                *bytes_read = bytes_read.saturating_add(bytes.len() as u64);
                parser.push(&bytes, output_notification)?;
            }
            host::StreamReadResult::WouldBlock => return Ok(false),
            host::StreamReadResult::Eof => return Ok(true),
        }
    }
}

fn drain_bounded_stderr(stream: u64, output: &mut Vec<u8>) -> Result<bool, String> {
    loop {
        match host::stream_read(stream, 4 * 1024)
            .map_err(|error| format!("read Neovim runner stderr: {error:?}"))?
        {
            host::StreamReadResult::Data(bytes) => {
                let remaining = (64_usize * 1024).saturating_sub(output.len());
                output.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            }
            host::StreamReadResult::WouldBlock => return Ok(false),
            host::StreamReadResult::Eof => return Ok(true),
        }
    }
}

fn take_runner_input_line() -> Result<Option<Vec<u8>>, String> {
    let Ok(mut guard) = STATE.lock() else {
        return Err("Neovim client state is poisoned".to_string());
    };
    let Some(state) = guard.as_mut() else {
        return Ok(None);
    };
    if let Some(input) = state.runner_queue.front_mut() {
        match input {
            RunnerInput::Line(_) => {
                let Some(RunnerInput::Line(line)) = state.runner_queue.pop_front() else {
                    return Err("runner input queue changed unexpectedly".to_string());
                };
                state.runner_queue_bytes = state.runner_queue_bytes.saturating_sub(line.len());
                return Ok(Some(line));
            }
            RunnerInput::Paste(paste) => {
                let (value, consumed, finished) = next_paste_record(paste)?;
                let line = encode_runner_line(&value)?;
                state.runner_queue_bytes = state.runner_queue_bytes.saturating_sub(consumed);
                if finished {
                    state.runner_queue.pop_front();
                }
                return Ok(Some(line));
            }
        }
    }
    loop {
        let Some(upload) = state.runner_uploads.front_mut() else {
            return Ok(None);
        };
        let value = match upload.phase {
            UploadPhase::Start => {
                upload.phase = UploadPhase::Chunks(0);
                json!({
                    "v": 1,
                    "type": "open-content-start",
                    "request_id": upload.request_id,
                    "path": upload.path,
                    "version": upload.version,
                    "size": upload.content.len(),
                    "line": upload.line,
                    "column": upload.column,
                    "readonly": false,
                })
            }
            UploadPhase::Chunks(offset) if offset < upload.content.len() => {
                let end = utf8_chunk_end(&upload.content, offset, RUNNER_OPEN_CHUNK_BYTES);
                let chunk = upload.content[offset..end].to_string();
                upload.phase = UploadPhase::Chunks(end);
                json!({
                    "v": 1,
                    "type": "open-content-chunk",
                    "request_id": upload.request_id,
                    "offset": offset,
                    "content": chunk,
                })
            }
            UploadPhase::Chunks(_) => {
                upload.phase = UploadPhase::End;
                continue;
            }
            UploadPhase::End => {
                let request_id = upload.request_id.clone();
                state.runner_uploads.pop_front();
                json!({
                    "v": 1,
                    "type": "open-content-end",
                    "request_id": request_id,
                })
            }
        };
        return encode_runner_line(&value)
            .map(Some)
            .map_err(|error| format!("generated Neovim runner upload: {error}"));
    }
}

fn encode_runner_line(value: &Value) -> Result<Vec<u8>, String> {
    let mut line =
        serde_json::to_vec(value).map_err(|error| format!("encode runner input: {error}"))?;
    if line.len() + 1 > MAX_RUNNER_LINE_BYTES {
        return Err("runner input exceeds the JSONL record bound".to_string());
    }
    line.push(b'\n');
    Ok(line)
}

fn next_paste_record(paste: &mut RunnerPaste) -> Result<(Value, usize, bool), String> {
    if paste.offset >= paste.text.len() {
        return Err("runner paste has no remaining input".to_string());
    }
    let start = paste.offset;
    let end = utf8_chunk_end(&paste.text, start, RUNNER_OPEN_CHUNK_BYTES);
    let finished = end == paste.text.len();
    let phase = if start == 0 && finished {
        -1
    } else if start == 0 {
        1
    } else if finished {
        3
    } else {
        2
    };
    let text = paste.text[start..end].to_string();
    paste.offset = end;
    Ok((
        json!({
            "v": 1,
            "type": "paste",
            "phase": phase,
            "text": text,
        }),
        end.saturating_sub(start),
        finished,
    ))
}

fn utf8_chunk_end(text: &str, offset: usize, max_bytes: usize) -> usize {
    let mut end = offset.saturating_add(max_bytes).min(text.len());
    while end > offset && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset && offset < text.len() {
        end = offset + text[offset..].chars().next().map_or(0, char::len_utf8);
    }
    end
}

fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<(usize, &str)> {
    if max_bytes == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut offset = 0;
    while offset < text.len() {
        let end = utf8_chunk_end(text, offset, max_bytes);
        chunks.push((offset, &text[offset..end]));
        offset = end;
    }
    chunks
}

fn runner_text_records(message_type: &str, text: &str) -> Vec<Value> {
    if !matches!(message_type, "text" | "input") {
        return Vec::new();
    }
    utf8_chunks(text, RUNNER_OPEN_CHUNK_BYTES)
        .into_iter()
        .map(|(_, chunk)| json!({"v": 1, "type": message_type, "text": chunk}))
        .collect()
}

fn nvim_key(logical: &str, modifiers: u32) -> Option<String> {
    let special = match logical {
        "Escape" | "Esc" => Some("Esc"),
        "Enter" | "Return" => Some("CR"),
        "Tab" => Some("Tab"),
        "Backspace" => Some("BS"),
        "Delete" => Some("Del"),
        "Insert" => Some("Insert"),
        "ArrowLeft" | "Left" => Some("Left"),
        "ArrowRight" | "Right" => Some("Right"),
        "ArrowUp" | "Up" => Some("Up"),
        "ArrowDown" | "Down" => Some("Down"),
        "Home" => Some("Home"),
        "End" => Some("End"),
        "PageUp" => Some("PageUp"),
        "PageDown" => Some("PageDown"),
        " " | "Space" if modifiers & (MOD_CTRL | MOD_ALT | MOD_SUPER) != 0 => Some("Space"),
        _ if logical.strip_prefix('F').is_some_and(|number| {
            number
                .parse::<u8>()
                .is_ok_and(|number| (1..=24).contains(&number))
        }) =>
        {
            Some(logical)
        }
        _ => None,
    };
    let has_command_modifier = modifiers & (MOD_CTRL | MOD_ALT | MOD_SUPER) != 0;
    if special.is_none() && !has_command_modifier {
        return None;
    }
    let base = special.map_or_else(
        || {
            if logical.chars().count() == 1 {
                logical.to_string()
            } else {
                logical.trim_matches(['<', '>']).to_string()
            }
        },
        str::to_string,
    );
    if base.is_empty() {
        return None;
    }
    let mut notation = String::from("<");
    if modifiers & MOD_CTRL != 0 {
        notation.push_str("C-");
    }
    if modifiers & MOD_ALT != 0 {
        notation.push_str("M-");
    }
    if modifiers & MOD_SHIFT != 0 {
        notation.push_str("S-");
    }
    if modifiers & MOD_SUPER != 0 {
        notation.push_str("D-");
    }
    notation.push_str(&base);
    notation.push('>');
    Some(notation)
}

fn decode_hex(encoded: &[u8]) -> Option<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    let mut decoded = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.chunks_exact(2) {
        decoded.push((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?);
    }
    Some(decoded)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

bindings::export!(NeovimClient with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_sender_authentication_is_exact() {
        assert!(trusted_file_sender("session:42:neovim-files", 42));
        assert!(!trusted_file_sender("session:41:neovim-files", 42));
        assert!(!trusted_file_sender("session:42:init", 42));
        assert!(!trusted_file_sender("session:42:neovim-files:spoofed", 42));
        assert!(!trusted_file_sender("presentation:neovim-files", 42));
        assert!(trusted_open_sender(CLIENT_INIT, 42));
        assert!(trusted_open_sender("session:42:init", 42));
        assert!(!trusted_open_sender("session:41:init", 42));
        assert!(!trusted_open_sender("session:42:init:spoofed", 42));
    }

    #[test]
    fn backend_chunks_round_trip_under_the_raw_limit() {
        let bytes = (0..BACKEND_CHUNK_BYTES * 2 + 37)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let mut decoded = Vec::new();
        for chunk in bytes.chunks(BACKEND_CHUNK_BYTES) {
            assert!(chunk.len() <= 7 * 1024);
            let encoded = encode_hex(chunk);
            assert_eq!(encoded.len(), chunk.len() * 2);
            decoded.extend(decode_hex(encoded.as_bytes()).expect("valid hex"));
        }
        assert_eq!(decoded, bytes);
        assert!(decode_hex(b"0").is_none());
        assert!(decode_hex(b"0x").is_none());
    }

    #[test]
    fn paths_and_open_coordinates_are_validated() {
        let open = parse_open_command(&json!({
            "kind": "editor-open",
            "path": "src/main.rs",
            "line": 7,
            "column": 3,
        }))
        .expect("valid open");
        assert_eq!(
            open,
            EditorOpen {
                path: "src/main.rs".to_string(),
                line: Some(7),
                column: Some(3),
            }
        );
        for path in [
            "",
            "/etc/passwd",
            "../secret",
            "src/../secret",
            "src\\main.rs",
        ] {
            assert!(!valid_relative_path(path), "{path:?}");
        }
        assert!(
            parse_open_command(&json!({
                "kind": "editor-open",
                "path": "src/main.rs",
                "line": 0,
            }))
            .is_none()
        );
        assert!(contains_nul(b"text\0bytes"));
        assert!(!contains_nul(b"text bytes"));
    }

    #[test]
    fn open_failures_use_the_typed_local_envelope_without_runner_input() {
        let mut state = ClientState::new(42, 7, "neovim-ui-b42-s7".to_string(), 1, 2);
        state.opens.insert(
            9,
            OpenTransfer {
                runner_request_id: "open-9".to_string(),
                path: "src/binary.dat".to_string(),
                line: None,
                column: None,
                revision: Some("r1".to_string()),
                size: Some(1),
                content: Vec::new(),
            },
        );
        let commands = handle_read_reply(
            &mut state,
            json!({
                "kind": "open",
                "request": 9,
                "path": "src/binary.dat",
                "revision": "r1",
                "size": 1,
                "offset": 0,
                "data_hex": "ff",
                "eof": true,
            })
            .as_object()
            .expect("reply object"),
        );
        let [Command::SendLocalMessage(message)] = commands.as_slice() else {
            panic!("expected one local failure message");
        };
        assert_eq!(message.actor, CLIENT_INIT);
        let envelope: Value = serde_json::from_slice(&message.body).expect("failure JSON");
        assert_eq!(envelope["kind"], "editor-open-failed");
        assert_eq!(envelope["source"], "neovim-client");
        assert_eq!(envelope["backend_id"], 42);
        assert_eq!(envelope["session_id"], 7);
        assert_eq!(envelope["path"], "src/binary.dat");
        assert!(state.runner_queue.is_empty());
    }

    #[test]
    fn abandoned_backend_transfers_are_explicitly_aborted() {
        let mut state = ClientState::new(42, 7, "neovim-ui-b42-s7".to_string(), 1, 2);
        state.opens.insert(
            9,
            OpenTransfer {
                runner_request_id: "open-9".to_string(),
                path: "src/main.rs".to_string(),
                line: None,
                column: None,
                revision: Some("r1".to_string()),
                size: Some(10),
                content: Vec::new(),
            },
        );
        let commands = handle_read_reply(
            &mut state,
            json!({
                "kind": "open",
                "request": 9,
                "path": "src/main.rs",
                "revision": "r1",
                "size": 10,
                "offset": 0,
                "data_hex": "not-hex",
                "eof": false,
            })
            .as_object()
            .expect("reply object"),
        );
        let [
            Command::SendSessionMessage(abort),
            Command::SendLocalMessage(failure),
        ] = commands.as_slice()
        else {
            panic!("expected backend abort followed by local failure");
        };
        let abort: Value = serde_json::from_slice(&abort.body).expect("abort JSON");
        assert_eq!(abort["kind"], "open-abort");
        assert_eq!(abort["request"], 9);
        assert_eq!(failure.actor, CLIENT_INIT);
        assert!(state.opens.is_empty());

        state.saves.insert(
            10,
            SaveTransfer {
                runner_request_id: "save-10".to_string(),
                transfer: 17,
                path: "src/main.rs".to_string(),
                content: b"new\n".to_vec(),
                next_offset: 0,
                inflight: 0,
                commit_sent: false,
            },
        );
        // Keep the native unit test out of the imported notification host
        // function while still exercising the backend abort command.
        state.runner_queue_bytes = MAX_RUNNER_QUEUE_BYTES;
        let commands = handle_save_ready(
            &mut state,
            json!({
                "kind": "save-ready",
                "request": 10,
                "transfer": 99,
                "path": "src/main.rs",
                "size": 4,
            })
            .as_object()
            .expect("reply object"),
        );
        let [Command::SendSessionMessage(abort)] = commands.as_slice() else {
            panic!("expected save abort");
        };
        let abort: Value = serde_json::from_slice(&abort.body).expect("abort JSON");
        assert_eq!(abort["kind"], "save-abort");
        assert_eq!(abort["request"], 10);
        assert_eq!(abort["transfer"], 17);
        assert!(state.saves.is_empty());
    }

    #[test]
    fn aggregate_state_allows_multiple_bounded_file_transfers() {
        let mut state = ClientState::new(42, 7, "neovim-ui-b42-s7".to_string(), 1, 2);
        state.runner_uploads.push_back(RunnerUpload {
            request_id: "open-1".to_string(),
            path: "src/file-1.txt".to_string(),
            version: None,
            content: "x".repeat(5 * 1024 * 1024),
            line: None,
            column: None,
            phase: UploadPhase::Start,
        });
        assert!(state.can_retain_file_bytes(5 * 1024 * 1024));
        state.runner_uploads.push_back(RunnerUpload {
            request_id: "open-2".to_string(),
            path: "src/file-2.txt".to_string(),
            version: None,
            content: "x".repeat(5 * 1024 * 1024),
            line: None,
            column: None,
            phase: UploadPhase::Start,
        });
        assert!(state.file_state_bytes() > MAX_FILE_BYTES);
        assert!(state.file_state_bytes() <= MAX_FILE_STATE_BYTES);
        assert!(!state.can_retain_file_bytes(MAX_FILE_STATE_BYTES));
    }

    #[test]
    fn presentation_grid_is_clamped_to_runner_dimensions() {
        let mut state = ClientState::new(1, 1, "neovim-ui-b1-s1".to_string(), 1, 2);
        state.width = 1.0;
        state.height = 1.0;
        assert_eq!(state.grid_size(), (MIN_RUNNER_ROWS, MIN_RUNNER_COLUMNS));
        state.width = 100_000.0;
        state.height = 100_000.0;
        assert_eq!(state.grid_size(), (MAX_RUNNER_ROWS, MAX_RUNNER_COLUMNS));
    }

    #[test]
    fn open_bursts_are_retained_and_started_in_order() {
        let mut state = ClientState::new(42, 7, "neovim-ui-b42-s7".to_string(), 1, 2);
        for index in 0..(MAX_TRANSFERS + MAX_PENDING_OPENS) {
            let commands = queue_or_start_open(
                &mut state,
                EditorOpen {
                    path: format!("src/file-{index}.rs"),
                    line: None,
                    column: None,
                },
            );
            assert_eq!(commands.len(), usize::from(index < MAX_TRANSFERS));
        }
        assert_eq!(state.opens.len(), MAX_TRANSFERS);
        assert_eq!(state.queued_opens.len(), MAX_PENDING_OPENS);

        let overflow = queue_or_start_open(
            &mut state,
            EditorOpen {
                path: "src/overflow.rs".to_string(),
                line: None,
                column: None,
            },
        );
        assert!(matches!(
            overflow.as_slice(),
            [Command::SendLocalMessage(_)]
        ));

        state.opens.remove(&1);
        let started = start_queued_opens(&mut state);
        let [Command::SendSessionMessage(message)] = started.as_slice() else {
            panic!("one queued open should start when one slot is released");
        };
        let request: Value = serde_json::from_slice(&message.body).expect("open request JSON");
        assert_eq!(request["path"], "src/file-4.rs");
        assert_eq!(state.opens.len(), MAX_TRANSFERS);
        assert_eq!(state.queued_opens.len(), MAX_PENDING_OPENS - 1);
    }

    #[test]
    fn save_completion_requires_exact_content_metadata() {
        let content = b"remote bytes\n".to_vec();
        let transfer = SaveTransfer {
            runner_request_id: "save-1".to_string(),
            transfer: 9,
            path: "src/main.rs".to_string(),
            next_offset: content.len(),
            inflight: 0,
            commit_sent: true,
            content,
        };
        let valid = json!({
            "kind": "save-complete",
            "request": 3,
            "transfer": 9,
            "path": "src/main.rs",
            "revision": content_revision(&transfer.content),
            "size": transfer.content.len(),
        });
        assert!(valid_save_completion(
            &transfer,
            valid.as_object().expect("completion object")
        ));
        for field in ["transfer", "path", "revision", "size"] {
            let mut invalid = valid.clone();
            invalid
                .as_object_mut()
                .expect("completion object")
                .remove(field);
            assert!(
                !valid_save_completion(&transfer, invalid.as_object().expect("completion object")),
                "missing {field} was accepted"
            );
        }
        let mut not_committed = transfer;
        not_committed.commit_sent = false;
        assert!(!valid_save_completion(
            &not_committed,
            valid.as_object().expect("completion object")
        ));
    }

    #[test]
    fn streamed_paste_preserves_utf8_and_one_transaction() {
        let text = "aé🙂z".repeat(900);
        let mut paste = RunnerPaste {
            text: text.clone(),
            offset: 0,
        };
        let mut rebuilt = String::new();
        let mut phases = Vec::new();
        loop {
            let (record, consumed, finished) = next_paste_record(&mut paste).expect("paste record");
            let chunk = record["text"].as_str().expect("paste text");
            assert_eq!(consumed, chunk.len());
            assert!(chunk.len() <= RUNNER_OPEN_CHUNK_BYTES);
            rebuilt.push_str(chunk);
            phases.push(record["phase"].as_i64().expect("paste phase"));
            if finished {
                break;
            }
        }
        assert_eq!(rebuilt, text);
        assert_eq!(phases.first(), Some(&1));
        assert_eq!(phases.last(), Some(&3));
        assert!(phases[1..phases.len() - 1].iter().all(|phase| *phase == 2));

        let mut short = RunnerPaste {
            text: "🙂".to_string(),
            offset: 0,
        };
        let (record, _, finished) = next_paste_record(&mut short).expect("short paste");
        assert!(finished);
        assert_eq!(record["phase"], -1);
    }

    #[test]
    fn terminal_notices_cannot_be_crowded_out_by_actions() {
        let mut state = ClientState::new(42, 7, "neovim-ui-b42-s7".to_string(), 1, 2);
        state.notices.extend(
            (0..MAX_RUNNER_NOTICES).map(|index| RunnerNotice::Action(json!({"index": index}))),
        );
        assert!(push_critical_notice(
            &mut state,
            RunnerNotice::OpenFailed {
                path: "src/main.rs".to_string(),
                message: "open failed".to_string(),
            }
        ));
        assert_eq!(state.notices.len(), MAX_RUNNER_NOTICES);
        assert!(state.notices.iter().any(|notice| matches!(
            notice,
            RunnerNotice::OpenFailed { path, .. } if path == "src/main.rs"
        )));
    }

    #[test]
    fn pane_keys_encode_as_neovim_notation_without_printable_duplicates() {
        assert_eq!(nvim_key("Escape", 0).as_deref(), Some("<Esc>"));
        assert_eq!(nvim_key("Enter", 0).as_deref(), Some("<CR>"));
        assert_eq!(nvim_key("Backspace", 0).as_deref(), Some("<BS>"));
        assert_eq!(nvim_key("Delete", 0).as_deref(), Some("<Del>"));
        assert_eq!(nvim_key("ArrowLeft", 0).as_deref(), Some("<Left>"));
        assert_eq!(nvim_key("ArrowLeft", MOD_CTRL).as_deref(), Some("<C-Left>"));
        assert_eq!(nvim_key("s", MOD_CTRL).as_deref(), Some("<C-s>"));
        assert_eq!(nvim_key(" ", 0), None);
        assert_eq!(nvim_key(" ", MOD_CTRL).as_deref(), Some("<C-Space>"));
        assert_eq!(
            nvim_key("x", MOD_CTRL | MOD_ALT | MOD_SHIFT).as_deref(),
            Some("<C-M-S-x>")
        );
        assert_eq!(nvim_key("x", 0), None);
    }

    #[test]
    fn ordinary_text_and_paste_use_distinct_runner_operations() {
        let typed = runner_text_records("text", "i🙂");
        let pasted = runner_text_records("input", "i🙂");
        assert_eq!(typed[0]["type"], "text");
        assert_eq!(pasted[0]["type"], "input");
        assert_eq!(typed[0]["text"], "i🙂");
        assert_eq!(pasted[0]["text"], "i🙂");
        assert!(runner_text_records("unknown", "ignored").is_empty());
    }

    #[test]
    fn utf8_runner_chunks_preserve_byte_offsets_and_boundaries() {
        let text = "aé🙂z".repeat(700);
        let chunks = utf8_chunks(&text, RUNNER_OPEN_CHUNK_BYTES);
        let mut rebuilt = String::new();
        let mut expected_offset = 0;
        for (offset, chunk) in chunks {
            assert_eq!(offset, expected_offset);
            assert!(chunk.len() <= RUNNER_OPEN_CHUNK_BYTES);
            expected_offset += chunk.len();
            rebuilt.push_str(chunk);
        }
        assert_eq!(rebuilt, text);
    }

    #[test]
    fn canonical_and_compatible_screen_fields_parse() {
        let canonical = parse_screen(&json!({
            "v": 1,
            "type": "screen",
            "revision": 9,
            "columns": 80,
            "rows": 24,
            "lines": ["one", "é🙂"],
            "cursor": {"row": 1, "column": 2, "visible": true},
            "mode": "normal",
            "buffer": {
                "path": "src/main.rs",
                "modified": true,
                "readonly": false
            },
            "cmdline": {"type": ":", "text": "write", "cursor": 5},
            "message": ""
        }))
        .expect("canonical screen");
        assert_eq!(canonical.revision, 9);
        assert_eq!(canonical.columns, 80);
        assert_eq!(canonical.rows, 24);
        assert_eq!(canonical.cursor_column, 2);
        assert_eq!(canonical.lines[1], "é🙂");
        assert_eq!(canonical.mode, "normal");
        assert_eq!(canonical.buffer_path, "src/main.rs");
        assert!(canonical.modified);
        assert!(!canonical.readonly);
        assert_eq!(canonical.cmdline, ":write");

        let compatible = parse_screen(&json!({
            "cols": 12,
            "rows": 2,
            "grid": [{"text": "hello"}, {"text": "world"}],
            "cursor": {"row": 0, "col": 4}
        }))
        .expect("compatible screen");
        assert_eq!(compatible.columns, 12);
        assert_eq!(compatible.cursor_column, 4);
    }
}
