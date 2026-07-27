wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use std::{collections::VecDeque, sync::Mutex};

use brood::plugin::host;
use exports::brood::plugin::actor::{
    Command, Event, Guest, LifecycleEvent, NotificationEvent, PresentationActorMessage,
    WorkerEvent, WorkerRequest,
};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

const WORKER: u64 = 1;
const SNAPSHOT_TOKEN: u64 = 1;
const STREAM_TOKEN: u64 = 2;
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;
const MAX_PENDING_INPUT: usize = 64 * 1024;
const MAX_SCROLLBACK: usize = 2_000;
const MAX_SNAPSHOT_BODY: usize = 12 * 1024;
const BULK_OUTPUT_CHUNK_BYTES: usize = 4 * 1024;
const SNAPSHOT_MAGIC: &[u8; 4] = b"BTRM";
const DEFAULT_FG: u32 = 0xebdbb2;
const DEFAULT_BG: u32 = 0x1d2021;

static STATE: Mutex<Option<SessionState>> = Mutex::new(None);

struct TerminalSession;

struct SessionState {
    session_id: u64,
    subscriber: Option<String>,
    snapshot_notification: u64,
    snapshot_generation: u64,
    stream_notification: Option<u64>,
    stream_generation: u64,
    child: Option<u64>,
    terminal: Option<u64>,
    pending_input: VecDeque<u8>,
    screen: Screen,
    viewport_offset: usize,
    revision: u64,
    chunks_read: u64,
    bytes_read: u64,
    resizes: u64,
    resize_output_batches: u8,
    snapshots_published: u64,
    exited: bool,
    exit_code: Option<i32>,
    error: Option<String>,
}

impl SessionState {
    fn new(session_id: u64, snapshot_notification: u64) -> Self {
        Self {
            session_id,
            subscriber: None,
            snapshot_notification,
            snapshot_generation: 0,
            stream_notification: None,
            stream_generation: 0,
            child: None,
            terminal: None,
            pending_input: VecDeque::new(),
            screen: Screen::new(INITIAL_ROWS, INITIAL_COLS),
            viewport_offset: 0,
            revision: 1,
            chunks_read: 0,
            bytes_read: 0,
            resizes: 0,
            resize_output_batches: 0,
            snapshots_published: 0,
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

    fn queue_input(&mut self, bytes: &[u8]) {
        let available = MAX_PENDING_INPUT.saturating_sub(self.pending_input.len());
        self.pending_input
            .extend(bytes.iter().copied().take(available));
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
                    self.pending_input.drain(..written as usize);
                }
                Ok(_) | Err(host::RuntimeIoError::WouldBlock) => {
                    self.watch_stream();
                    return;
                }
                Err(error) => {
                    self.error = Some(format!("terminal input failed: {error:?}"));
                    self.pending_input.clear();
                    self.mark_changed();
                    return;
                }
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
        let Some(actor) = self.subscriber.clone() else {
            return Vec::new();
        };
        let chunks = encode_snapshot(self);
        self.snapshots_published = self.snapshots_published.saturating_add(chunks.len() as u64);
        chunks
            .into_iter()
            .map(|body| {
                Command::SendPresentationMessage(PresentationActorMessage {
                    actor: actor.clone(),
                    body,
                })
            })
            .collect()
    }
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
                if let Ok(mut state) = STATE.lock() {
                    *state = Some(SessionState::new(session_id, notification));
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
                state.snapshot_commands()
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
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
            state.snapshot_commands()
        }
        Some(b'I') if sender.starts_with("presentation:") => {
            let revision = state.revision;
            state.viewport_offset = 0;
            state.queue_input(body.get(1..).unwrap_or_default());
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
        _ => Vec::new(),
    }
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
            let commands = state.snapshot_commands();
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

async fn run_terminal_worker(notification: u64) -> Result<Vec<u8>, String> {
    let (rows, cols) = {
        let guard = STATE
            .lock()
            .map_err(|_| "terminal state is poisoned".to_string())?;
        let state = guard
            .as_ref()
            .ok_or_else(|| "terminal state is unavailable".to_string())?;
        (state.screen.rows, state.screen.cols)
    };
    let pty = host::pty_spawn(
        "sh",
        &["-i".to_string()],
        host::PtySpawnOptions {
            rows,
            cols,
            terminal_profile: host::TerminalProfile::Xterm256color,
        },
    )
    .map_err(|error| format!("spawn terminal shell: {error:?}"))?;
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
                if bytes.len() >= BULK_OUTPUT_CHUNK_BYTES
                    || !alternate_screen_active()?
                    || take_resize_output_batch()?
                {
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
            "{{\"session\":{},\"chunks\":{},\"bytes\":{},\"revision\":{},\"exit\":\"{:?}\",\"code\":{}}}",
            state.session_id,
            state.chunks_read,
            state.bytes_read,
            state.revision,
            exit_state,
            exit_code.map_or_else(|| "null".to_string(), |code| code.to_string()),
        )
    };
    let _ = host::stream_drop(pty.terminal).await;
    let _ = host::child_drop(pty.child).await;
    Ok(metrics.into_bytes())
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

async fn wait_timer(timer: u64) -> Result<(), String> {
    let notification = host::timer_notification(timer)
        .map_err(|error| format!("snapshot timer notification: {error:?}"))?;
    let mut generation = 0;
    while host::timer_status(timer).map_err(|error| format!("snapshot timer status: {error:?}"))?
        == host::TimerState::Pending
    {
        generation = wait_notification(notification, generation).await?;
    }
    Ok(())
}

async fn write_worker_bytes(stream: u64, mut bytes: &[u8]) -> Result<(), String> {
    let notification = host::stream_notification(stream)
        .map_err(|error| format!("stream notification: {error:?}"))?;
    let mut observed = 0;
    while !bytes.is_empty() {
        match host::stream_write(stream, bytes) {
            Ok(written) if written > 0 => bytes = &bytes[written as usize..],
            Ok(_) | Err(host::RuntimeIoError::WouldBlock) => {
                observed = wait_notification(notification, observed).await?;
            }
            Err(error) => return Err(format!("write terminal response: {error:?}")),
        }
    }
    Ok(())
}

async fn wait_notification(notification: u64, generation: u64) -> Result<u64, String> {
    let pollable = host::notification_pollable(notification, generation)
        .map_err(|error| format!("create terminal pollable: {error:?}"))?;
    let ready = host::worker_wait_any(vec![pollable], None, host::WaitMode::Fair)
        .await
        .map_err(|error| format!("wait for terminal readiness: {error:?}"));
    let _ = host::pollable_drop(pollable);
    ready.map(|ready| ready.generation)
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
}

impl Screen {
    fn new(rows: u16, cols: u16) -> Self {
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
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let next = resize_rows(&self.cells, rows, cols);
        if let Some((main, _, _)) = self.alternate.as_mut() {
            *main = resize_rows(main, rows, cols);
        }
        self.rows = rows;
        self.cols = cols;
        self.cells = next;
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
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
        for col in 0..self.cols {
            let valid = col > 0 && {
                let previous = &self.cells[usize::from(row)][usize::from(col - 1)];
                !previous.continuation
                    && previous
                        .text
                        .chars()
                        .map(|character| UnicodeWidthChar::width(character).unwrap_or(1))
                        .sum::<usize>()
                        == 2
            };
            let cell = &mut self.cells[usize::from(row)][usize::from(col)];
            if cell.continuation && !valid {
                reset_cell(cell, self.style);
            }
        }
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
            let col = self.cursor_col.saturating_sub(1);
            self.cells[usize::from(self.cursor_row)][usize::from(col)]
                .text
                .push(character);
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
        self.cells[usize::from(self.cursor_row)][usize::from(self.cursor_col)] = Cell {
            text: character.to_string(),
            continuation: false,
            style: self.style,
        };
        if width == 2 && self.cursor_col + 1 < self.cols {
            self.cells[usize::from(self.cursor_row)][usize::from(self.cursor_col + 1)] = Cell {
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

fn resize_rows(cells: &[Vec<Cell>], rows: u16, cols: u16) -> Vec<Vec<Cell>> {
    let mut next = blank_rows(rows, cols, Style::default());
    let copy_rows = usize::from(rows).min(cells.len());
    for row in 0..copy_rows {
        let copy_cols = usize::from(cols).min(cells[row].len());
        for col in 0..copy_cols {
            next[row][col] = cells[row][col].clone();
        }
    }
    next
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

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

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
            'u' => (self.cursor_row, self.cursor_col) = self.saved_cursor,
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
            b'c' => {
                let rows = self.rows;
                let cols = self.cols;
                *self = Self::new(rows, cols);
            }
            _ => {}
        }
    }
}

fn ansi_color(index: u16, bright: bool) -> u32 {
    const NORMAL: [u32; 8] = [
        0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
    ];
    const BRIGHT: [u32; 8] = [
        0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
    ];
    let index = usize::from(index.min(7));
    if bright { BRIGHT[index] } else { NORMAL[index] }
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

fn encode_snapshot(state: &SessionState) -> Vec<Vec<u8>> {
    let mut row_blobs = Vec::with_capacity(usize::from(state.screen.rows));
    for row in 0..state.screen.rows {
        row_blobs.push(encode_row(&state.screen, state.viewport_offset, row));
    }
    let mut groups = Vec::<(u16, Vec<Vec<u8>>)>::new();
    let mut start = 0_u16;
    let mut current = Vec::new();
    let mut current_bytes = 72_usize;
    for (row, blob) in row_blobs.into_iter().enumerate() {
        if !current.is_empty() && current_bytes.saturating_add(blob.len()) > MAX_SNAPSHOT_BODY {
            groups.push((start, std::mem::take(&mut current)));
            start = row as u16;
            current_bytes = 72;
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
            let mut bytes = Vec::with_capacity(72 + rows.iter().map(Vec::len).sum::<usize>());
            bytes.extend_from_slice(SNAPSHOT_MAGIC);
            bytes.push(2);
            bytes.extend_from_slice(&state.revision.to_le_bytes());
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

export!(TerminalSession);
