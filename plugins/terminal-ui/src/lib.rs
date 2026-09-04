use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{Arc, Mutex},
};

use brood_graphics::{Color, Command as DrawCommand, Packet, Rect, theme::ashen};
use brood_plugin_sdk::{
    bindings,
    brood::plugin::host,
    exports::brood::plugin::actor::{
        Command, Event, Guest, LifecycleEvent, PresentationEvent, SessionActorMessage, WorkerEvent,
        WorkerRequest,
    },
    graphics::queue_encoded_packet_for_resize_with_state_watermark,
    input::{
        BUTTON_MIDDLE as MIDDLE_BUTTON, BUTTON_PRIMARY as LEFT_BUTTON, MOD_ALT, MOD_CTRL, MOD_SHIFT,
    },
    runtime::sleep as wait_timer,
    ui::{
        self, MANAGED_UI_PROTOCOL_VERSION, UiAccessibility, UiAccessibilityRole, UiDocument,
        UiNode, UiNodeKind, UiPublication, UiStyle,
    },
};
use unicode_width::UnicodeWidthChar;

const SNAPSHOT_MAGIC: &[u8; 4] = b"BTRM";
const SNAPSHOT_VERSION: u8 = 4;
const SNAPSHOT_FULL: u8 = 1;
const FONT_SIZE: f32 = 16.0;
const REQUESTED_LINE_HEIGHT: f32 = 20.0;
const PADDING: f32 = 8.0;
const DEFAULT_BG: u32 = ashen::BACKGROUND_RGB;
const CURSOR: u32 = ashen::CURSOR_RGB;
const SELECTION_BG: u32 = ashen::SELECTION_RGB;
const SCROLL_PIXELS_PER_LINE: f32 = 5.0;
const MOUSE_WHEEL_PIXELS_PER_CLICK: f32 = 15.0;
const MAX_CACHED_LAYOUTS: usize = 512;
// Presentation events are already latest-state coalesced and render requests
// are paced by host hints. Keep only a minimal burst guard here so live resize
// can drive genuine PTY reflow instead of snapping at the old 24 ms cadence.
const RESIZE_DEBOUNCE_MS: u64 = 1;
const RESIZE_WORKER_PAYLOAD: &[u8] = b"terminal-resize-debounce";
const CURSOR_BLINK_MS: u64 = 530;
const CURSOR_BLINK_WORKER_PAYLOAD: &[u8] = b"terminal-cursor-blink";
const PRESENTATION_BATCH_LIMIT: u8 = 16;
// `focus_commands_for_mailbox` is not a presentation reducer. Every branch in
// `handle_presentation` returns at most three commands, including clipboard
// and resize paths; dispatch may append one coalesced render-metrics command.
// One opted-in batch therefore remains within the host's aggregate 64-command
// dispatch ceiling.
const MAX_PRESENTATION_COMMANDS_PER_EVENT: usize = 4;
const MAX_PRESENTATION_BATCH_COMMANDS: usize =
    PRESENTATION_BATCH_LIMIT as usize * MAX_PRESENTATION_COMMANDS_PER_EVENT;
const _: () = assert!(MAX_PRESENTATION_BATCH_COMMANDS <= 64);
const CLIENT_INIT_ID: &str = "brood.client-init";
const INBOX_ID: &str = "inbox";
const FOCUS_MESSAGE: &[u8] = br#"{"kind":"focus"}"#;

static STATE: Mutex<Option<UiState>> = Mutex::new(None);

struct TerminalUi;

struct UiState {
    pane: u64,
    focus_on_create: bool,
    backend_id: u64,
    session_id: u64,
    session_actor: String,
    width: f32,
    height: f32,
    cell_width: f32,
    line_height: f32,
    assembly: Option<SnapshotAssembly>,
    latest: Option<Arc<Snapshot>>,
    next_worker: u64,
    cache: HashMap<RunKey, CachedLayout>,
    layouts_created: u64,
    layouts_reused: u64,
    layouts_dropped: u64,
    packets: u64,
    snapshots_received: u64,
    snapshot_chunks_received: u64,
    snapshot_bytes_received: u64,
    snapshot_rows_received: u64,
    full_snapshots_received: u64,
    metric_application_cursor: Option<bool>,
    metric_bracketed_paste: Option<bool>,
    metric_resizes: u64,
    metric_viewport_offset: usize,
    metric_bytes_read: u64,
    metric_exited: bool,
    max_packet_bytes: usize,
    max_commands: usize,
    requested_viewport: usize,
    selection: Option<Selection>,
    pressed_mouse_button: Option<u8>,
    mouse_modifiers: u32,
    scroll_accumulator: f32,
    mouse_wheel_accumulator: f32,
    suppressed_text: Option<String>,
    resize_worker_pending: bool,
    last_sent_grid_size: Option<(u16, u16)>,
    next_input_sequence: u64,
    pending_resize_epoch: Option<u64>,
    focused: bool,
    cursor_blink_on: bool,
    cursor_blink_worker: Option<u64>,
    // Pure rendering cannot emit actor commands. Retain only its newest
    // diagnostic sample and publish it from the next ordinary actor turn.
    pending_render_metrics: Option<Vec<u8>>,
}

impl UiState {
    fn new(backend_id: u64, session_id: u64, instance_id: &str) -> Self {
        let session_actor = terminal_session_actor(instance_id);
        Self {
            pane: 0,
            focus_on_create: terminal_focuses_on_create(instance_id),
            backend_id,
            session_id,
            session_actor,
            width: 640.0,
            height: 480.0,
            cell_width: 9.0,
            line_height: REQUESTED_LINE_HEIGHT,
            assembly: None,
            latest: None,
            next_worker: 1,
            cache: HashMap::new(),
            layouts_created: 0,
            layouts_reused: 0,
            layouts_dropped: 0,
            packets: 0,
            snapshots_received: 0,
            snapshot_chunks_received: 0,
            snapshot_bytes_received: 0,
            snapshot_rows_received: 0,
            full_snapshots_received: 0,
            metric_application_cursor: None,
            metric_bracketed_paste: None,
            metric_resizes: 0,
            metric_viewport_offset: 0,
            metric_bytes_read: 0,
            metric_exited: false,
            max_packet_bytes: 0,
            max_commands: 0,
            requested_viewport: 0,
            selection: None,
            pressed_mouse_button: None,
            mouse_modifiers: 0,
            scroll_accumulator: 0.0,
            mouse_wheel_accumulator: 0.0,
            suppressed_text: None,
            resize_worker_pending: false,
            last_sent_grid_size: None,
            next_input_sequence: 0,
            pending_resize_epoch: None,
            // Focus authority arrives through PaneFocused. Assuming focus here
            // makes every background terminal schedule blink renders during
            // startup, before the compositor has selected a pane.
            focused: false,
            cursor_blink_on: true,
            cursor_blink_worker: None,
            pending_render_metrics: None,
        }
    }

    fn grid_size(&self) -> (u16, u16) {
        let rows = ((self.height - PADDING * 2.0) / self.line_height)
            .floor()
            .clamp(1.0, 4_096.0) as u16;
        let cols = ((self.width - PADDING * 2.0) / self.cell_width)
            .floor()
            .clamp(1.0, 4_096.0) as u16;
        (rows, cols)
    }

    fn session_message(&self, body: Vec<u8>) -> Command {
        Command::SendSessionMessage(SessionActorMessage {
            backend_id: self.backend_id,
            session_id: self.session_id,
            actor: self.session_actor.clone(),
            body,
        })
    }

    fn input_command(&mut self, bytes: &[u8]) -> Command {
        self.next_input_sequence = self.next_input_sequence.saturating_add(1).max(1);
        let mut body = Vec::with_capacity(bytes.len() + 9);
        body.push(b'I');
        body.extend_from_slice(&self.next_input_sequence.to_le_bytes());
        body.extend_from_slice(bytes);
        self.session_message(body)
    }

    fn resize_command(
        rows: u16,
        cols: u16,
        backend_id: u64,
        session_id: u64,
        actor: String,
    ) -> Command {
        let mut body = vec![b'R'];
        body.extend_from_slice(&rows.to_le_bytes());
        body.extend_from_slice(&cols.to_le_bytes());
        Command::SendSessionMessage(SessionActorMessage {
            backend_id,
            session_id,
            actor,
            body,
        })
    }

    fn send_resize_if_changed(&mut self) -> Option<Command> {
        let grid_size = self.grid_size();
        if self.last_sent_grid_size == Some(grid_size) {
            return None;
        }
        self.last_sent_grid_size = Some(grid_size);
        Some(Self::resize_command(
            grid_size.0,
            grid_size.1,
            self.backend_id,
            self.session_id,
            self.session_actor.clone(),
        ))
    }

    fn schedule_resize(&mut self) -> Option<Command> {
        if self.resize_worker_pending {
            return None;
        }
        self.resize_worker_pending = true;
        let worker = self.next_worker;
        self.next_worker = self.next_worker.saturating_add(1).max(1);
        Some(Command::SpawnWorker(WorkerRequest {
            worker,
            payload: RESIZE_WORKER_PAYLOAD.to_vec(),
        }))
    }

    fn schedule_cursor_blink(&mut self) -> Option<Command> {
        if self.cursor_blink_worker.is_some() || !self.focused {
            return None;
        }
        let worker = self.next_worker;
        self.next_worker = self.next_worker.saturating_add(1).max(1);
        self.cursor_blink_worker = Some(worker);
        Some(Command::SpawnWorker(WorkerRequest {
            worker,
            payload: CURSOR_BLINK_WORKER_PAYLOAD.to_vec(),
        }))
    }

    fn cancel_cursor_blink(&mut self) -> Option<Command> {
        self.cursor_blink_worker.take().map(Command::CancelWorker)
    }

    fn restart_cursor_blink(&mut self) -> Vec<Command> {
        if !self.cursor_blink_on {
            self.cursor_blink_on = true;
            let _ignored = brood_plugin_sdk::graphics::request_render();
        }
        // A presentation batch can contain hundreds of key events. Cancelling and
        // replacing the timer for every event briefly retains every cancelled guest
        // worker and can exhaust the bounded worker table before completions drain.
        // Keep the one existing deadline; input still makes the cursor immediately
        // visible and the next blink schedules the following bounded deadline.
        self.schedule_cursor_blink().into_iter().collect()
    }

    fn viewport_command(&self, offset: usize) -> Command {
        let mut body = vec![b'V'];
        body.extend_from_slice(&u32::try_from(offset).unwrap_or(u32::MAX).to_le_bytes());
        self.session_message(body)
    }

    fn metrics_command_if_due(&mut self, payload: Vec<u8>) -> Option<Command> {
        let snapshot = self.latest.as_ref()?;
        let state_changed = self.metric_application_cursor != Some(snapshot.application_cursor)
            || self.metric_bracketed_paste != Some(snapshot.bracketed_paste)
            || self.metric_resizes != snapshot.resizes
            || self.metric_viewport_offset != snapshot.viewport_offset
            || snapshot.bytes_read.saturating_sub(self.metric_bytes_read) >= 512
            || self.metric_exited != snapshot.exited;
        if self.packets > 3 && !self.packets.is_multiple_of(8) && !state_changed {
            return None;
        }
        self.metric_application_cursor = Some(snapshot.application_cursor);
        self.metric_bracketed_paste = Some(snapshot.bracketed_paste);
        self.metric_resizes = snapshot.resizes;
        self.metric_viewport_offset = snapshot.viewport_offset;
        self.metric_bytes_read = snapshot.bytes_read;
        self.metric_exited = snapshot.exited;
        let worker = self.next_worker;
        self.next_worker = self.next_worker.saturating_add(1).max(1);
        Some(Command::SpawnWorker(WorkerRequest { worker, payload }))
    }
}

fn terminal_session_actor(instance_id: &str) -> String {
    if instance_id == "neovim-ui" || instance_id.starts_with("neovim-ui-") {
        return "neovim-session".to_string();
    }
    if let Some(scoped) = instance_id.strip_prefix("terminal-ui-b")
        && let Some((_backend, session_and_suffix)) = scoped.split_once('-')
        && let Some(session_and_suffix) = session_and_suffix.strip_prefix('s')
    {
        return session_and_suffix.split_once('-').map_or_else(
            || "terminal-session".to_string(),
            |(_session, suffix)| format!("terminal-session-{suffix}"),
        );
    }
    if let Some(scoped) = instance_id.strip_prefix("terminal-ui-s")
        && let Some((_session, suffix)) = scoped.split_once('-')
    {
        return format!("terminal-session-{suffix}");
    }
    instance_id.strip_prefix("terminal-ui-").map_or_else(
        || "terminal-session".to_string(),
        |suffix| format!("terminal-session-{suffix}"),
    )
}

fn terminal_focuses_on_create(instance_id: &str) -> bool {
    instance_id.starts_with("terminal-ui")
        && terminal_session_actor(instance_id) != "terminal-session"
}

struct Snapshot {
    revision: u64,
    input_sequence: u64,
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    cursor_visible: bool,
    application_cursor: bool,
    bracketed_paste: bool,
    mouse_tracking: u8,
    mouse_sgr: bool,
    exited: bool,
    exit_code: Option<i32>,
    chunks_read: u64,
    bytes_read: u64,
    resizes: u64,
    viewport_offset: usize,
    max_viewport_offset: usize,
    rows_data: Vec<Vec<Run>>,
}

struct SnapshotAssembly {
    header: SnapshotHeader,
    chunks: Vec<Option<SnapshotRowsChunk>>,
}

type SnapshotRowsChunk = Vec<(u16, Vec<Run>)>;

#[derive(Clone, Copy)]
struct SnapshotHeader {
    revision: u64,
    input_sequence: u64,
    full: bool,
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    cursor_visible: bool,
    application_cursor: bool,
    bracketed_paste: bool,
    mouse_tracking: u8,
    mouse_sgr: bool,
    exited: bool,
    exit_code: Option<i32>,
    chunks_read: u64,
    bytes_read: u64,
    resizes: u64,
    viewport_offset: usize,
    max_viewport_offset: usize,
}

#[derive(Clone)]
struct Run {
    start: u16,
    cells: u16,
    fg: u32,
    bg: u32,
    bold: bool,
    faint: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CellPoint {
    row: u16,
    col: u16,
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    anchor: CellPoint,
    extent: CellPoint,
    dragging: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RunKey {
    cells: u16,
    fg: u32,
    bold: bool,
    faint: bool,
    italic: bool,
    text: String,
}

struct CachedLayout {
    handle: u64,
    last_used_revision: u64,
}

fn terminal_text_clusters(text: &str) -> Vec<(u16, u16, String)> {
    let mut clusters = Vec::<(u16, u16, String)>::new();
    let mut cell = 0_u16;
    for character in text.trim_end().chars() {
        let width = UnicodeWidthChar::width(character).unwrap_or(1) as u16;
        if width == 0
            && let Some((_, _, cluster)) = clusters.last_mut()
        {
            cluster.push(character);
            continue;
        }
        let width = width.max(1);
        clusters.push((cell, width, character.to_string()));
        cell = cell.saturating_add(width);
    }
    clusters
}

#[allow(
    clippy::too_many_arguments,
    reason = "terminal text style and geometry define one immutable cached layout"
)]
fn terminal_text_layout(
    cache: &mut HashMap<RunKey, CachedLayout>,
    pane: u64,
    run: &Run,
    text: String,
    cells: u16,
    cell_width: f32,
    line_height: f32,
    revision: u64,
    created: &mut u64,
    reused: &mut u64,
) -> Result<u64, String> {
    let key = RunKey {
        cells,
        fg: run.fg,
        bold: run.bold,
        faint: run.faint,
        italic: run.italic,
        text: text.clone(),
    };
    match cache.entry(key) {
        Entry::Occupied(mut entry) => {
            *reused = reused.saturating_add(1);
            entry.get_mut().last_used_revision = revision;
            Ok(entry.get().handle)
        }
        Entry::Vacant(entry) => {
            let color = rgb_channels(run.fg);
            let layout = host::graphics_layout_text(
                pane,
                &text,
                host::TextLayoutOptions {
                    font_intent: host::FontIntent::Monospace,
                    font_weight: if run.bold {
                        host::FontWeight::Bold
                    } else {
                        host::FontWeight::Normal
                    },
                    font_style: if run.italic {
                        host::FontStyle::Italic
                    } else {
                        host::FontStyle::Normal
                    },
                    font_size: FONT_SIZE,
                    max_width: (f32::from(cells) * cell_width).max(cell_width),
                    line_height,
                    red: color[0],
                    green: color[1],
                    blue: color[2],
                    alpha: if run.faint { 0.6 } else { 1.0 },
                },
            )
            .map_err(|error| format!("layout terminal text: {error:?}"))?;
            entry.insert(CachedLayout {
                handle: layout.handle,
                last_used_revision: revision,
            });
            *created = created.saturating_add(1);
            Ok(layout.handle)
        }
    }
}

impl Guest for TerminalUi {
    fn render(
        request: brood_plugin_sdk::exports::brood::plugin::actor::PureRenderRequest,
    ) -> Result<
        brood_plugin_sdk::exports::brood::plugin::actor::PureRenderResult,
        brood_plugin_sdk::exports::brood::plugin::actor::PureRenderError,
    > {
        use brood_plugin_sdk::exports::brood::plugin::actor::PureRenderError;
        let prior = {
            let Ok(mut guard) = STATE.lock() else {
                return Err(PureRenderError::Busy);
            };
            let Some(state) = guard.as_mut() else {
                return Err(PureRenderError::InvalidRequest);
            };
            let Some(target) = request.panes.iter().find(|target| target.pane == state.pane) else {
                return Err(PureRenderError::InvalidRequest);
            };
            let prior = (state.width, state.height);
            state.width = target.logical_width;
            state.height = target.logical_height;
            prior
        };
        let mut metrics = None;
        let result = brood_plugin_sdk::graphics::render_pure(&request, || {
            metrics = Some(render_latest_snapshot(None)?);
            Ok(())
        });
        if let Ok(mut guard) = STATE.lock()
            && let Some(state) = guard.as_mut()
        {
            (state.width, state.height) = prior;
            if let Some(metrics) = metrics {
                state.pending_render_metrics = Some(metrics);
            }
        }
        result
    }
    fn dispatch(event: Event) -> Vec<Command> {
        let mut commands = match event {
            Event::Lifecycle(LifecycleEvent::Start(context)) => {
                let Some(backend_id) = context.backend_id else {
                    return vec![Command::LifecycleComplete(Err(
                        "terminal UI requires a backend association".to_string(),
                    ))];
                };
                let Some(session_id) = context.session_id else {
                    return vec![Command::LifecycleComplete(Err(
                        "terminal UI has no bound session".to_string(),
                    ))];
                };
                if let Ok(mut state) = STATE.lock() {
                    *state = Some(UiState::new(backend_id, session_id, &context.instance_id));
                }
                vec![
                    Command::SetPresentationBatchLimit(PRESENTATION_BATCH_LIMIT),
                    Command::LifecycleComplete(Ok(())),
                ]
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![Command::LifecycleComplete(Ok(()))]
            }
            Event::Presentation(event) => handle_presentation(event),
            Event::PresentationBatch(events) => {
                events.into_iter().flat_map(handle_presentation).collect()
            }
            Event::Mailbox(message)
                if message.sender.starts_with("session:")
                    && message.body.starts_with(SNAPSHOT_MAGIC) =>
            {
                handle_snapshot_chunk(&message.body)
            }
            Event::Mailbox(message) => focus_commands_for_mailbox(&message.sender, &message.body),
            Event::Worker(WorkerEvent::Finished(finished))
                if finished.output == RESIZE_WORKER_PAYLOAD =>
            {
                finish_resize_debounce()
            }
            Event::Worker(WorkerEvent::Failed(failed))
                if failed.message.starts_with("terminal resize debounce:") =>
            {
                finish_resize_debounce()
            }
            Event::Worker(WorkerEvent::Finished(finished))
                if finished.output == CURSOR_BLINK_WORKER_PAYLOAD =>
            {
                finish_cursor_blink(finished.worker)
            }
            Event::Worker(WorkerEvent::Failed(failed))
                if failed.message.starts_with("terminal cursor blink:") =>
            {
                finish_cursor_blink(failed.worker)
            }
            Event::Worker(WorkerEvent::Cancelled(worker)) => cancel_cursor_blink_finished(worker),
            Event::Worker(WorkerEvent::Finished(_)) | Event::Worker(WorkerEvent::Failed(_)) => {
                Vec::new()
            }
            _ => Vec::new(),
        };
        if let Ok(mut guard) = STATE.lock()
            && let Some(state) = guard.as_mut()
            && let Some(metrics) = state.pending_render_metrics.take()
            && let Some(command) = state.metrics_command_if_due(metrics)
        {
            commands.push(command);
        }
        commands
    }

    async fn worker_run(_worker: u64, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        if payload == RESIZE_WORKER_PAYLOAD {
            wait_timer(RESIZE_DEBOUNCE_MS)
                .await
                .map_err(|error| format!("terminal resize debounce: {error}"))?;
        } else if payload == CURSOR_BLINK_WORKER_PAYLOAD {
            wait_timer(CURSOR_BLINK_MS)
                .await
                .map_err(|error| format!("terminal cursor blink: {error}"))?;
        }
        Ok(payload)
    }
}

fn focus_commands_for_mailbox(sender: &str, body: &[u8]) -> Vec<Command> {
    if !matches!(sender, CLIENT_INIT_ID | INBOX_ID) || body != FOCUS_MESSAGE {
        return Vec::new();
    }
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut().filter(|state| state.pane != 0) else {
        return Vec::new();
    };
    let (rows, cols) = state.grid_size();
    vec![
        Command::SendHostMessage(
            format!(r#"{{"type":"focus-pane","pane":{}}}"#, state.pane).into_bytes(),
        ),
        state.session_message(vec![b'A']),
        UiState::resize_command(
            rows,
            cols,
            state.backend_id,
            state.session_id,
            state.session_actor.clone(),
        ),
    ]
}

fn handle_presentation(event: PresentationEvent) -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    match event {
        PresentationEvent::PaneCreated(created) => {
            state.pane = created.pane;
            state.width = created.width.max(1.0);
            state.height = created.height.max(1.0);
            if let Ok(metrics) = host::graphics_layout_text(
                state.pane,
                "M",
                host::TextLayoutOptions {
                    font_intent: host::FontIntent::Monospace,
                    font_weight: host::FontWeight::Normal,
                    font_style: host::FontStyle::Normal,
                    font_size: FONT_SIZE,
                    max_width: 64.0,
                    line_height: REQUESTED_LINE_HEIGHT,
                    red: 1.0,
                    green: 1.0,
                    blue: 1.0,
                    alpha: 1.0,
                },
            ) {
                state.cell_width = metrics.cell_width.max(1.0);
                state.line_height = metrics.line_height.max(1.0);
                let _ = host::graphics_drop_resource(state.pane, metrics.handle);
            }
            let mut commands = terminal_ready_commands(state);
            if let Ok(command) = terminal_document(state.pane) {
                commands.push(command);
            }
            commands
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            state.width = resized.width.max(1.0);
            state.height = resized.height.max(1.0);
            state.pending_resize_epoch = resized.resize_epoch;
            let commands = state.schedule_resize().into_iter().collect();
            if state.latest.is_some() {
                let _ignored = brood_plugin_sdk::graphics::request_render_for_resize(
                    state.pane,
                    resized.resize_epoch,
                );
            }
            commands
        }
        PresentationEvent::PaneFocused(focused) if focused.pane == state.pane => {
            let mut commands = state.cancel_cursor_blink().into_iter().collect::<Vec<_>>();
            state.focused = focused.focused;
            state.cursor_blink_on = true;
            let _ignored = brood_plugin_sdk::graphics::request_render();
            commands.extend(state.schedule_cursor_blink());
            commands
        }
        PresentationEvent::Key(key) if key.pane == state.pane && key.pressed => {
            let mut commands = state.restart_cursor_blink();
            state.suppressed_text = ((key.modifiers & (MOD_CTRL | MOD_ALT)) != 0
                && key.logical_key.chars().count() == 1)
                .then(|| key.logical_key.clone());
            let copy_shortcut = key.modifiers & (MOD_CTRL | MOD_SHIFT) == (MOD_CTRL | MOD_SHIFT)
                && key.logical_key.eq_ignore_ascii_case("c");
            if copy_shortcut {
                commands.extend(
                    state
                        .selection
                        .and_then(|selection| selection_text(state.latest.as_deref(), selection))
                        .and_then(set_clipboard_command),
                );
                return commands;
            }
            let paste_shortcut = (key.modifiers & (MOD_CTRL | MOD_SHIFT) == (MOD_CTRL | MOD_SHIFT)
                && key.logical_key.eq_ignore_ascii_case("v"))
                || (key.modifiers & MOD_SHIFT != 0 && key.logical_key == "Insert");
            if paste_shortcut {
                commands.push(request_clipboard_paste_command(state.pane));
                return commands;
            }
            let page = state
                .latest
                .as_ref()
                .map_or(1, |snapshot| usize::from(snapshot.rows.saturating_sub(1)));
            let max_viewport = state
                .latest
                .as_ref()
                .map_or(0, |snapshot| snapshot.max_viewport_offset);
            let viewport = match (key.modifiers & MOD_SHIFT != 0, key.logical_key.as_str()) {
                (true, "PageUp") => Some(
                    state
                        .requested_viewport
                        .saturating_add(page)
                        .min(max_viewport),
                ),
                (true, "PageDown") => Some(state.requested_viewport.saturating_sub(page)),
                (true, "Home") => Some(max_viewport),
                (true, "End") => Some(0),
                _ => None,
            };
            if let Some(viewport) = viewport {
                state.requested_viewport = viewport;
                commands.push(state.viewport_command(viewport));
                return commands;
            }
            let application_cursor = state
                .latest
                .as_ref()
                .is_some_and(|snapshot| snapshot.application_cursor);
            commands.extend(
                key_bytes(&key.logical_key, key.modifiers, application_cursor).map(|bytes| {
                    state.selection = None;
                    state.requested_viewport = 0;
                    state.input_command(&bytes)
                }),
            );
            commands
        }
        PresentationEvent::Text(text) if text.pane == state.pane && !text.text.is_empty() => {
            let mut commands = state.restart_cursor_blink();
            if state
                .suppressed_text
                .take()
                .is_some_and(|key| key.eq_ignore_ascii_case(&text.text))
            {
                return commands;
            }
            let printable = text
                .text
                .chars()
                .filter(|character| !character.is_control())
                .collect::<String>();
            if printable.is_empty() {
                commands
            } else {
                state.selection = None;
                state.requested_viewport = 0;
                commands.push(state.input_command(printable.as_bytes()));
                commands
            }
        }
        PresentationEvent::Paste(paste) if paste.pane == state.pane && !paste.text.is_empty() => {
            let mut commands = state.restart_cursor_blink();
            let bracketed = state
                .latest
                .as_ref()
                .is_some_and(|snapshot| snapshot.bracketed_paste);
            state.selection = None;
            state.requested_viewport = 0;
            commands.push(state.input_command(&paste_bytes(&paste.text, bracketed)));
            commands
        }
        PresentationEvent::PointerMoved(moved) if moved.pane == state.pane => {
            if state
                .selection
                .as_ref()
                .is_some_and(|selection| selection.dragging)
            {
                let point = pointer_cell(state, moved.x, moved.y);
                let changed = state.selection.as_mut().is_some_and(|selection| {
                    let changed = selection.extent != point;
                    selection.extent = point;
                    changed
                });
                if changed {
                    let _ignored = brood_plugin_sdk::graphics::request_render();
                }
                return Vec::new();
            }
            let Some(snapshot) = state.latest.as_ref() else {
                return Vec::new();
            };
            if snapshot.mouse_tracking < 2
                || (snapshot.mouse_tracking == 2 && state.pressed_mouse_button.is_none())
            {
                return Vec::new();
            }
            let button = state.pressed_mouse_button.unwrap_or(3).saturating_add(32);
            mouse_report(
                pointer_cell(state, moved.x, moved.y),
                button,
                true,
                state.mouse_modifiers,
                snapshot.mouse_sgr,
            )
            .map(|bytes| state.input_command(&bytes))
            .into_iter()
            .collect()
        }
        PresentationEvent::PointerButton(button) if button.pane == state.pane => {
            let tracking = state
                .latest
                .as_ref()
                .map_or(0, |snapshot| snapshot.mouse_tracking);
            let local_pointer = tracking == 0 || button.modifiers & MOD_SHIFT != 0;
            if local_pointer && button.button == MIDDLE_BUTTON && button.pressed {
                return vec![request_clipboard_paste_command(state.pane)];
            }
            if local_pointer && button.button == LEFT_BUTTON {
                let point = pointer_cell(state, button.x, button.y);
                let mut clipboard = None;
                if button.pressed {
                    state.selection = Some(Selection {
                        anchor: point,
                        extent: point,
                        dragging: true,
                    });
                } else if let Some(selection) = state.selection.as_mut() {
                    selection.extent = point;
                    selection.dragging = false;
                    clipboard = selection_text(state.latest.as_deref(), *selection);
                }
                let _ignored = brood_plugin_sdk::graphics::request_render();
                return clipboard
                    .and_then(set_clipboard_command)
                    .into_iter()
                    .collect();
            }
            if local_pointer {
                return Vec::new();
            }
            let Some(mouse_button) = mouse_button_number(button.button) else {
                return Vec::new();
            };
            let mouse_sgr = state
                .latest
                .as_ref()
                .is_some_and(|snapshot| snapshot.mouse_sgr);
            let reported_button = if button.pressed {
                state.pressed_mouse_button = Some(mouse_button);
                state.mouse_modifiers = button.modifiers;
                mouse_button
            } else {
                state.pressed_mouse_button = None;
                if mouse_sgr { mouse_button } else { 3 }
            };
            mouse_report(
                pointer_cell(state, button.x, button.y),
                reported_button,
                button.pressed,
                button.modifiers,
                mouse_sgr,
            )
            .map(|bytes| state.input_command(&bytes))
            .into_iter()
            .collect()
        }
        PresentationEvent::PointerScroll(scroll) if scroll.pane == state.pane => {
            let Some(snapshot) = state.latest.as_ref() else {
                return Vec::new();
            };
            if snapshot.mouse_tracking > 0 && scroll.modifiers & MOD_SHIFT == 0 {
                state.mouse_wheel_accumulator += scroll.vertical / MOUSE_WHEEL_PIXELS_PER_CLICK;
                let clicks = state.mouse_wheel_accumulator.trunc() as isize;
                state.mouse_wheel_accumulator -= clicks as f32;
                if clicks == 0 {
                    return Vec::new();
                }
                let point = pointer_cell(state, scroll.x, scroll.y);
                let button = if clicks > 0 { 64 } else { 65 };
                let mut bytes = Vec::new();
                for _ in 0..clicks.unsigned_abs().min(64) {
                    if let Some(report) =
                        mouse_report(point, button, true, scroll.modifiers, snapshot.mouse_sgr)
                    {
                        bytes.extend_from_slice(&report);
                    }
                }
                return (!bytes.is_empty())
                    .then(|| state.input_command(&bytes))
                    .into_iter()
                    .collect();
            }
            state.scroll_accumulator += scroll.vertical / SCROLL_PIXELS_PER_LINE;
            let delta = state.scroll_accumulator.trunc() as isize;
            state.scroll_accumulator -= delta as f32;
            if delta == 0 {
                return Vec::new();
            }
            let next = state
                .requested_viewport
                .saturating_add_signed(delta)
                .min(snapshot.max_viewport_offset);
            if next == state.requested_viewport {
                Vec::new()
            } else {
                state.requested_viewport = next;
                vec![state.viewport_command(next)]
            }
        }
        PresentationEvent::RenderHint(hint) => {
            drop(guard);
            let Ok(metrics) = brood_plugin_sdk::graphics::render_with_hint(&hint, || {
                render_latest_snapshot(None)
            }) else {
                return Vec::new();
            };
            let Ok(mut guard) = STATE.lock() else {
                return Vec::new();
            };
            let Some(state) = guard.as_mut() else {
                return Vec::new();
            };
            state.metrics_command_if_due(metrics).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

fn terminal_document(pane: u64) -> Result<Command, serde_json::Error> {
    ui::publish(UiPublication::Snapshot(Box::new(UiDocument {
        protocol_version: MANAGED_UI_PROTOCOL_VERSION,
        pane,
        document_revision: 1,
        root: UiNode {
            id: 1,
            kind: UiNodeKind::TerminalView { terminal: pane },
            style: UiStyle {
                grow: 1.0,
                ..UiStyle::default()
            },
            accessibility: UiAccessibility {
                role: UiAccessibilityRole::Terminal,
                label: Some("Terminal".to_string()),
                description: Some("Interactive terminal session".to_string()),
                ..UiAccessibility::default()
            },
            optional: false,
            interaction_revision: 0,
            children: Vec::new(),
        },
    })))
}

fn terminal_ready_commands(state: &mut UiState) -> Vec<Command> {
    let mut commands = Vec::with_capacity(3);
    if state.focus_on_create {
        commands.push(Command::SendHostMessage(
            format!(r#"{{"type":"focus-pane","pane":{}}}"#, state.pane).into_bytes(),
        ));
    }
    commands.push(state.session_message(vec![b'A']));
    commands.push(
        state
            .send_resize_if_changed()
            .expect("new terminal pane must have an initial grid size"),
    );
    commands
}

fn finish_resize_debounce() -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    state.resize_worker_pending = false;
    state.send_resize_if_changed().into_iter().collect()
}

fn finish_cursor_blink(worker: u64) -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    let (redraw, commands) = finish_cursor_blink_for_state(state, worker);
    if redraw {
        let _ignored = brood_plugin_sdk::graphics::request_render();
    }
    commands
}

fn finish_cursor_blink_for_state(state: &mut UiState, worker: u64) -> (bool, Vec<Command>) {
    if state.cursor_blink_worker != Some(worker) {
        return (false, Vec::new());
    }
    state.cursor_blink_worker = None;
    if !state.focused {
        let redraw = !state.cursor_blink_on;
        state.cursor_blink_on = true;
        return (redraw, Vec::new());
    }
    state.cursor_blink_on = !state.cursor_blink_on;
    (true, state.schedule_cursor_blink().into_iter().collect())
}

fn cancel_cursor_blink_finished(worker: u64) -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    if state.cursor_blink_worker == Some(worker) {
        state.cursor_blink_worker = None;
    }
    Vec::new()
}

fn set_clipboard_command(text: String) -> Option<Command> {
    serde_json::to_vec(&serde_json::json!({
        "type": "set-clipboard",
        "text": text,
    }))
    .ok()
    .map(Command::SendHostMessage)
}

fn request_clipboard_paste_command(pane: u64) -> Command {
    Command::SendHostMessage(
        format!(r#"{{"type":"request-clipboard-paste","pane":{pane}}}"#).into_bytes(),
    )
}

fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if bracketed {
        let mut bytes = Vec::with_capacity(normalized.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(normalized.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        bytes
    } else {
        normalized.into_bytes()
    }
}

fn pointer_cell(state: &UiState, x: f32, y: f32) -> CellPoint {
    let (rows, cols) = state.latest.as_ref().map_or_else(
        || state.grid_size(),
        |snapshot| (snapshot.rows, snapshot.cols),
    );
    let row = ((y - PADDING) / state.line_height)
        .floor()
        .clamp(0.0, f32::from(rows.saturating_sub(1))) as u16;
    let col = ((x - PADDING) / state.cell_width)
        .floor()
        .clamp(0.0, f32::from(cols.saturating_sub(1))) as u16;
    CellPoint { row, col }
}

const fn mouse_button_number(button: u32) -> Option<u8> {
    match button {
        0x110 => Some(0),
        0x112 => Some(1),
        0x111 => Some(2),
        _ => None,
    }
}

fn mouse_report(
    point: CellPoint,
    button: u8,
    pressed: bool,
    modifiers: u32,
    sgr: bool,
) -> Option<Vec<u8>> {
    let modifier = 4 * u8::from(modifiers & MOD_SHIFT != 0)
        + 8 * u8::from(modifiers & MOD_ALT != 0)
        + 16 * u8::from(modifiers & MOD_CTRL != 0);
    let button = button.saturating_add(modifier);
    let col = point.col.saturating_add(1);
    let row = point.row.saturating_add(1);
    if sgr {
        let suffix = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{button};{col};{row}{suffix}").into_bytes());
    }
    if col > 223 || row > 223 {
        return None;
    }
    Some(vec![
        0x1b,
        b'[',
        b'M',
        button.saturating_add(32),
        u8::try_from(col).ok()?.saturating_add(32),
        u8::try_from(row).ok()?.saturating_add(32),
    ])
}

fn selection_text(snapshot: Option<&Snapshot>, selection: Selection) -> Option<String> {
    let snapshot = snapshot?;
    let (start, end) = normalized_selection(selection);
    let mut lines = Vec::new();
    for row in start.row..=end.row {
        let cells = snapshot_row_cells(snapshot, row);
        let first = if row == start.row { start.col } else { 0 };
        let last = if row == end.row {
            end.col
        } else {
            snapshot.cols.saturating_sub(1)
        };
        let mut line = String::new();
        for col in first..=last {
            if let Some(text) = cells.get(usize::from(col)) {
                line.push_str(text);
            }
        }
        lines.push(line.trim_end_matches(' ').to_string());
    }
    let text = lines.join("\n");
    (!text.is_empty()).then_some(text)
}

fn snapshot_row_cells(snapshot: &Snapshot, row: u16) -> Vec<String> {
    let Some(runs) = snapshot.rows_data.get(usize::from(row)) else {
        return vec![" ".to_string(); usize::from(snapshot.cols)];
    };
    row_cells(snapshot.cols, runs)
}

fn row_cells(cols: u16, runs: &[Run]) -> Vec<String> {
    let mut cells = vec![" ".to_string(); usize::from(cols)];
    for run in runs {
        let mut col = usize::from(run.start);
        let mut lead_col: Option<usize> = None;
        for character in run.text.chars() {
            let width = UnicodeWidthChar::width(character).unwrap_or(1);
            if width == 0 {
                if let Some(lead) = lead_col
                    && let Some(cell) = cells.get_mut(lead)
                {
                    cell.push(character);
                }
                continue;
            }
            if let Some(cell) = cells.get_mut(col) {
                cell.clear();
                cell.push(character);
                lead_col = Some(col);
            } else {
                lead_col = None;
            }
            for continuation in 1..width {
                if let Some(cell) = cells.get_mut(col.saturating_add(continuation)) {
                    cell.clear();
                }
            }
            col = col.saturating_add(width);
        }
    }
    cells
}

fn normalized_selection(selection: Selection) -> (CellPoint, CellPoint) {
    if selection.anchor <= selection.extent {
        (selection.anchor, selection.extent)
    } else {
        (selection.extent, selection.anchor)
    }
}

fn selection_columns(selection: Selection, row: u16, cols: u16) -> Option<(u16, u16)> {
    let (start, end) = normalized_selection(selection);
    if row < start.row || row > end.row {
        return None;
    }
    let first = if row == start.row { start.col } else { 0 };
    let last = if row == end.row {
        end.col
    } else {
        cols.saturating_sub(1)
    };
    Some((first.min(cols), last.saturating_add(1).min(cols)))
}

fn key_bytes(logical: &str, modifiers: u32, application_cursor: bool) -> Option<Vec<u8>> {
    let modifier = 1
        + u8::from(modifiers & MOD_SHIFT != 0)
        + 2 * u8::from(modifiers & MOD_ALT != 0)
        + 4 * u8::from(modifiers & MOD_CTRL != 0);
    let modified = modifier > 1;
    let cursor = |final_byte: char| {
        if modified {
            format!("\x1b[1;{modifier}{final_byte}").into_bytes()
        } else if application_cursor {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[{final_byte}").into_bytes()
        }
    };
    let tilde = |number: u8| {
        if modified {
            format!("\x1b[{number};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{number}~").into_bytes()
        }
    };
    let function = |number: u8, final_byte: char| {
        if modified {
            format!("\x1b[1;{modifier}{final_byte}").into_bytes()
        } else if number <= 4 {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            tilde(number)
        }
    };
    match logical {
        "Enter" | "Return" | "NumpadEnter" => Some(b"\r".to_vec()),
        "Escape" => Some(b"\x1b".to_vec()),
        "Backspace" => Some(if modifiers & MOD_ALT != 0 {
            b"\x1b\x7f".to_vec()
        } else {
            b"\x7f".to_vec()
        }),
        "Tab" if modifiers & MOD_SHIFT != 0 => Some(b"\x1b[Z".to_vec()),
        "Tab" => Some(b"\t".to_vec()),
        "ArrowUp" => Some(cursor('A')),
        "ArrowDown" => Some(cursor('B')),
        "ArrowRight" => Some(cursor('C')),
        "ArrowLeft" => Some(cursor('D')),
        "Home" => Some(cursor('H')),
        "End" => Some(cursor('F')),
        "Insert" => Some(tilde(2)),
        "Delete" => Some(tilde(3)),
        "PageUp" => Some(tilde(5)),
        "PageDown" => Some(tilde(6)),
        "F1" => Some(function(1, 'P')),
        "F2" => Some(function(2, 'Q')),
        "F3" => Some(function(3, 'R')),
        "F4" => Some(function(4, 'S')),
        "F5" => Some(tilde(15)),
        "F6" => Some(tilde(17)),
        "F7" => Some(tilde(18)),
        "F8" => Some(tilde(19)),
        "F9" => Some(tilde(20)),
        "F10" => Some(tilde(21)),
        "F11" => Some(tilde(23)),
        "F12" => Some(tilde(24)),
        _ if modifiers & MOD_CTRL != 0 => {
            let character = logical.chars().next()?.to_ascii_lowercase();
            let byte = match character {
                'a'..='z' => (character as u8) & 0x1f,
                '@' | ' ' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '7' | '/' => 0x1f,
                '?' | '8' => 0x7f,
                _ => return None,
            };
            Some(if modifiers & MOD_ALT != 0 {
                vec![0x1b, byte]
            } else {
                vec![byte]
            })
        }
        _ if modifiers & MOD_ALT != 0 && logical.chars().count() == 1 => {
            let mut bytes = vec![0x1b];
            bytes.extend_from_slice(logical.as_bytes());
            Some(bytes)
        }
        _ => None,
    }
}

fn handle_snapshot_chunk(body: &[u8]) -> Vec<Command> {
    let Ok((header, chunk_index, chunk_count, row_start, rows)) = decode_chunk(body) else {
        return Vec::new();
    };
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    state.snapshot_chunks_received = state.snapshot_chunks_received.saturating_add(1);
    state.snapshot_bytes_received = state
        .snapshot_bytes_received
        .saturating_add(body.len() as u64);
    state.snapshot_rows_received = state
        .snapshot_rows_received
        .saturating_add(rows.len() as u64);
    if state
        .latest
        .as_ref()
        .is_some_and(|snapshot| snapshot.revision >= header.revision)
    {
        return Vec::new();
    }
    if state
        .assembly
        .as_ref()
        .is_none_or(|assembly| assembly.header.revision != header.revision)
    {
        if !header.full
            && state
                .latest
                .as_ref()
                .is_none_or(|snapshot| snapshot.rows != header.rows || snapshot.cols != header.cols)
        {
            return Vec::new();
        }
        state.assembly = Some(SnapshotAssembly {
            header,
            chunks: vec![None; usize::from(chunk_count)],
        });
    }
    let Some(assembly) = state.assembly.as_mut() else {
        return Vec::new();
    };
    if assembly.chunks.len() != usize::from(chunk_count)
        || usize::from(chunk_index) >= assembly.chunks.len()
    {
        state.assembly = None;
        return Vec::new();
    }
    assembly.chunks[usize::from(chunk_index)] = Some(
        rows.into_iter()
            .enumerate()
            .map(|(offset, runs)| (row_start.saturating_add(offset as u16), runs))
            .collect(),
    );
    if assembly.chunks.iter().any(Option::is_none) {
        return Vec::new();
    }
    let Some(assembly) = state.assembly.take() else {
        return Vec::new();
    };
    let mut rows_data = if header.full {
        vec![Vec::new(); usize::from(header.rows)]
    } else {
        state
            .latest
            .as_ref()
            .map(|snapshot| snapshot.rows_data.clone())
            .unwrap_or_default()
    };
    let mut updated = vec![false; usize::from(header.rows)];
    for chunk in assembly.chunks.into_iter().flatten() {
        for (row, runs) in chunk {
            let row = usize::from(row);
            if let Some(target) = rows_data.get_mut(row) {
                *target = runs;
                updated[row] = true;
            }
        }
    }
    if header.full && updated.iter().any(|updated| !updated) {
        return Vec::new();
    }
    state.latest = Some(Arc::new(Snapshot {
        revision: header.revision,
        input_sequence: header.input_sequence,
        rows: header.rows,
        cols: header.cols,
        cursor_row: header.cursor_row,
        cursor_col: header.cursor_col,
        cursor_visible: header.cursor_visible,
        application_cursor: header.application_cursor,
        bracketed_paste: header.bracketed_paste,
        mouse_tracking: header.mouse_tracking,
        mouse_sgr: header.mouse_sgr,
        exited: header.exited,
        exit_code: header.exit_code,
        chunks_read: header.chunks_read,
        bytes_read: header.bytes_read,
        resizes: header.resizes,
        viewport_offset: header.viewport_offset,
        max_viewport_offset: header.max_viewport_offset,
        rows_data,
    }));
    state.snapshots_received = state.snapshots_received.saturating_add(1);
    state.full_snapshots_received = state
        .full_snapshots_received
        .saturating_add(u64::from(header.full));
    state.requested_viewport = header.viewport_offset;
    let resize_epoch = state.pending_resize_epoch;
    let _ignored = brood_plugin_sdk::graphics::request_render_for_resize(state.pane, resize_epoch);
    state.schedule_cursor_blink().into_iter().collect()
}

fn render_latest_snapshot(resize_epoch: Option<u64>) -> Result<Vec<u8>, String> {
    let (
        pane,
        width,
        height,
        cell_width,
        line_height,
        snapshot,
        selection,
        cursor_blink_on,
        mut cache,
    ) = {
        let mut guard = STATE
            .lock()
            .map_err(|_| "terminal UI state is poisoned".to_string())?;
        let state = guard
            .as_mut()
            .ok_or_else(|| "terminal UI state is unavailable".to_string())?;
        let snapshot = state
            .latest
            .clone()
            .ok_or_else(|| "terminal UI has no snapshot".to_string())?;
        (
            state.pane,
            state.width,
            state.height,
            state.cell_width,
            state.line_height,
            snapshot,
            state.selection,
            state.cursor_blink_on || !state.focused,
            std::mem::take(&mut state.cache),
        )
    };

    let mut packet = Packet::new();
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, 0.0, width, height),
        color: rgb(DEFAULT_BG),
    });
    let mut created = 0_u64;
    let mut reused = 0_u64;
    for (row, runs) in snapshot.rows_data.iter().enumerate() {
        let y = PADDING + row as f32 * line_height;
        for run in runs {
            if run.bg != DEFAULT_BG {
                packet.push(DrawCommand::FillRect {
                    rect: Rect::new(
                        PADDING + f32::from(run.start) * cell_width,
                        y,
                        f32::from(run.cells) * cell_width,
                        line_height,
                    ),
                    color: rgb(run.bg),
                });
            }
        }
        if let Some((selection_start, selection_end)) =
            selection.and_then(|selection| selection_columns(selection, row as u16, snapshot.cols))
            && selection_start < selection_end
        {
            packet.push(DrawCommand::FillRect {
                rect: Rect::new(
                    PADDING + f32::from(selection_start) * cell_width,
                    y,
                    f32::from(selection_end - selection_start) * cell_width,
                    line_height,
                ),
                color: rgb(SELECTION_BG),
            });
        }
        for run in runs {
            let x = PADDING + f32::from(run.start) * cell_width;
            let run_width = f32::from(run.cells) * cell_width;
            let text = run.text.trim_end();
            if text.is_empty() {
                continue;
            }
            if row as u16 == snapshot.cursor_row {
                for (offset, cells, text) in terminal_text_clusters(text) {
                    if text == " " {
                        continue;
                    }
                    let handle = terminal_text_layout(
                        &mut cache,
                        pane,
                        run,
                        text,
                        cells,
                        cell_width,
                        line_height,
                        snapshot.revision,
                        &mut created,
                        &mut reused,
                    )
                    .map_err(|error| format!("layout terminal cursor row: {error}"))?;
                    packet.push(DrawCommand::DrawTextLayout {
                        layout: handle,
                        x: x + f32::from(offset) * cell_width,
                        y,
                    });
                }
            } else {
                let cells = text
                    .chars()
                    .map(|character| UnicodeWidthChar::width(character).unwrap_or(1))
                    .sum::<usize>()
                    .max(1)
                    .min(usize::from(run.cells)) as u16;
                let handle = terminal_text_layout(
                    &mut cache,
                    pane,
                    run,
                    text.to_string(),
                    cells,
                    cell_width,
                    line_height,
                    snapshot.revision,
                    &mut created,
                    &mut reused,
                )?;
                packet.push(DrawCommand::DrawTextLayout {
                    layout: handle,
                    x,
                    y,
                });
            }
            if run.underline {
                packet.push(DrawCommand::FillRect {
                    rect: Rect::new(x, y + line_height - 2.0, run_width, 1.0),
                    color: rgb(run.fg),
                });
            }
            if run.strikethrough {
                packet.push(DrawCommand::FillRect {
                    rect: Rect::new(x, y + line_height * 0.55, run_width, 1.0),
                    color: rgb(run.fg),
                });
            }
        }
    }
    if snapshot.cursor_visible
        && cursor_blink_on
        && snapshot.cursor_row < snapshot.rows
        && snapshot.cursor_col < snapshot.cols
    {
        packet.push(DrawCommand::StrokeRect {
            rect: Rect::new(
                PADDING + f32::from(snapshot.cursor_col) * cell_width,
                PADDING + f32::from(snapshot.cursor_row) * line_height,
                cell_width,
                line_height,
            ),
            color: rgb(CURSOR),
            width: 1.0,
        });
    }
    if snapshot.exited {
        packet.push(DrawCommand::FillRect {
            rect: Rect::new(0.0, height - 3.0, width, 3.0),
            color: rgb(ashen::ERROR_RGB),
        });
    }
    if snapshot.max_viewport_offset > 0 {
        let total = snapshot
            .max_viewport_offset
            .saturating_add(usize::from(snapshot.rows));
        let track_height = (height - PADDING * 2.0).max(1.0);
        let thumb_height =
            (track_height * f32::from(snapshot.rows) / total as f32).clamp(12.0, track_height);
        let travel = (track_height - thumb_height).max(0.0);
        let from_top = snapshot
            .max_viewport_offset
            .saturating_sub(snapshot.viewport_offset);
        let thumb_y =
            PADDING + travel * from_top as f32 / snapshot.max_viewport_offset.max(1) as f32;
        packet.push(DrawCommand::FillRect {
            rect: Rect::new(width - 3.0, thumb_y, 2.0, thumb_height),
            color: rgb(CURSOR),
        });
    }
    let command_count = packet.commands.len();
    let bytes = packet
        .encode()
        .map_err(|error| format!("encode terminal draw packet: {error}"))?;
    let packet_bytes = bytes.len();
    let submit_result = queue_encoded_packet_for_resize_with_state_watermark(
        pane,
        resize_epoch,
        Some(snapshot.input_sequence),
        bytes,
    )
    .map_err(|error| format!("submit terminal draw packet: {error}"));
    if let Err(error) = submit_result {
        if let Ok(mut guard) = STATE.lock()
            && let Some(state) = guard.as_mut()
        {
            state.cache = cache;
        }
        return Err(error);
    }
    let drop_count = cache.len().saturating_sub(MAX_CACHED_LAYOUTS);
    let mut obsolete = cache
        .iter()
        .filter(|(_, cached)| cached.last_used_revision != snapshot.revision)
        .map(|(key, cached)| (cached.last_used_revision, key.clone()))
        .collect::<Vec<_>>();
    obsolete.sort_by_key(|(revision, _)| *revision);
    let mut dropped = 0_u64;
    for (_, key) in obsolete.into_iter().take(drop_count) {
        let Some(cached) = cache.get(&key) else {
            continue;
        };
        match host::graphics_drop_resource(pane, cached.handle) {
            Ok(()) | Err(host::GraphicsError::InvalidResource) => {
                cache.remove(&key);
                dropped = dropped.saturating_add(1);
            }
            Err(_) => {}
        }
    }

    let (
        created_total,
        reused_total,
        dropped_total,
        packets_total,
        snapshots_received,
        snapshot_chunks_received,
        snapshot_bytes_received,
        snapshot_rows_received,
        full_snapshots_received,
        cache_entries,
    ) = {
        let mut guard = STATE
            .lock()
            .map_err(|_| "terminal UI state is poisoned".to_string())?;
        let state = guard
            .as_mut()
            .ok_or_else(|| "terminal UI state is unavailable".to_string())?;
        state.cache = cache;
        state.layouts_created = state.layouts_created.saturating_add(created);
        state.layouts_reused = state.layouts_reused.saturating_add(reused);
        state.layouts_dropped = state.layouts_dropped.saturating_add(dropped);
        state.packets = state.packets.saturating_add(1);
        state.max_packet_bytes = state.max_packet_bytes.max(packet_bytes);
        state.max_commands = state.max_commands.max(command_count);
        if resize_epoch.is_some() && state.pending_resize_epoch == resize_epoch {
            state.pending_resize_epoch = None;
        }
        (
            state.layouts_created,
            state.layouts_reused,
            state.layouts_dropped,
            state.packets,
            state.snapshots_received,
            state.snapshot_chunks_received,
            state.snapshot_bytes_received,
            state.snapshot_rows_received,
            state.full_snapshots_received,
            state.cache.len(),
        )
    };
    let snapshots_coalesced = snapshots_received.saturating_sub(packets_total);
    Ok(format!(
        "{{\"revision\":{},\"input_sequence\":{},\"rows\":{},\"cols\":{},\"packet_bytes\":{},\"commands\":{},\"layouts_created\":{},\"layouts_reused\":{},\"layouts_dropped\":{},\"layouts_created_total\":{},\"layouts_reused_total\":{},\"layouts_dropped_total\":{},\"layout_cache_entries\":{},\"packets_total\":{},\"snapshots_received\":{},\"snapshots_coalesced\":{},\"snapshot_chunks_received\":{},\"snapshot_bytes_received\":{},\"snapshot_rows_received\":{},\"full_snapshots_received\":{},\"pty_chunks\":{},\"pty_bytes\":{},\"pty_resizes\":{},\"viewport_offset\":{},\"max_viewport_offset\":{},\"application_cursor\":{},\"bracketed_paste\":{},\"mouse_tracking\":{},\"mouse_sgr\":{},\"exited\":{},\"exit_code\":{}}}",
        snapshot.revision,
        snapshot.input_sequence,
        snapshot.rows,
        snapshot.cols,
        packet_bytes,
        command_count,
        created,
        reused,
        dropped,
        created_total,
        reused_total,
        dropped_total,
        cache_entries,
        packets_total,
        snapshots_received,
        snapshots_coalesced,
        snapshot_chunks_received,
        snapshot_bytes_received,
        snapshot_rows_received,
        full_snapshots_received,
        snapshot.chunks_read,
        snapshot.bytes_read,
        snapshot.resizes,
        snapshot.viewport_offset,
        snapshot.max_viewport_offset,
        snapshot.application_cursor,
        snapshot.bracketed_paste,
        snapshot.mouse_tracking,
        snapshot.mouse_sgr,
        snapshot.exited,
        snapshot
            .exit_code
            .map_or_else(|| "null".to_string(), |code| code.to_string()),
    )
    .into_bytes())
}

type DecodedChunk = (SnapshotHeader, u16, u16, u16, Vec<Vec<Run>>);

fn decode_chunk(bytes: &[u8]) -> Result<DecodedChunk, ()> {
    let mut reader = Reader::new(bytes);
    if reader.take(4)? != SNAPSHOT_MAGIC || reader.u8()? != SNAPSHOT_VERSION {
        return Err(());
    }
    let snapshot_flags = reader.u8()?;
    if snapshot_flags & !SNAPSHOT_FULL != 0 {
        return Err(());
    }
    let revision = reader.u64()?;
    let input_sequence = reader.u64()?;
    let rows = reader.u16()?;
    let cols = reader.u16()?;
    let cursor_row = reader.u16()?;
    let cursor_col = reader.u16()?;
    let cursor_visible = reader.u8()? != 0;
    let screen_flags = reader.u8()?;
    let exited = reader.u8()? != 0;
    let raw_exit = reader.i32()?;
    let chunk_index = reader.u16()?;
    let chunk_count = reader.u16()?;
    let row_start = reader.u16()?;
    let row_count = reader.u16()?;
    let chunks_read = reader.u64()?;
    let bytes_read = reader.u64()?;
    let resizes = reader.u64()?;
    let viewport_offset = reader.u32()? as usize;
    let max_viewport_offset = reader.u32()? as usize;
    if rows == 0
        || cols == 0
        || chunk_count == 0
        || chunk_index >= chunk_count
        || viewport_offset > max_viewport_offset
    {
        return Err(());
    }
    let mut decoded_rows = Vec::with_capacity(usize::from(row_count));
    for _ in 0..row_count {
        let run_count = reader.u16()?;
        let mut runs = Vec::with_capacity(usize::from(run_count));
        for _ in 0..run_count {
            let start = reader.u16()?;
            let cells = reader.u16()?;
            let fg = reader.u32()?;
            let bg = reader.u32()?;
            let style = reader.u8()?;
            let text_len = reader.u16()?;
            let text = std::str::from_utf8(reader.take(usize::from(text_len))?)
                .map_err(|_| ())?
                .to_string();
            if start.saturating_add(cells) > cols {
                return Err(());
            }
            runs.push(Run {
                start,
                cells,
                fg,
                bg,
                bold: style & 1 != 0,
                faint: style & 2 != 0,
                italic: style & 4 != 0,
                underline: style & 8 != 0,
                strikethrough: style & 16 != 0,
                text,
            });
        }
        decoded_rows.push(runs);
    }
    if !reader.remaining().is_empty() {
        return Err(());
    }
    Ok((
        SnapshotHeader {
            revision,
            input_sequence,
            full: snapshot_flags & SNAPSHOT_FULL != 0,
            rows,
            cols,
            cursor_row,
            cursor_col,
            cursor_visible,
            application_cursor: screen_flags & 1 != 0,
            bracketed_paste: screen_flags & 2 != 0,
            mouse_tracking: (screen_flags >> 2) & 3,
            mouse_sgr: screen_flags & 16 != 0,
            exited,
            exit_code: (raw_exit != i32::MIN).then_some(raw_exit),
            chunks_read,
            bytes_read,
            resizes,
            viewport_offset,
            max_viewport_offset,
        },
        chunk_index,
        chunk_count,
        row_start,
        decoded_rows,
    ))
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ()> {
        let end = self.offset.checked_add(count).ok_or(())?;
        let value = self.bytes.get(self.offset..end).ok_or(())?;
        self.offset = end;
        Ok(value)
    }

    fn remaining(&self) -> &'a [u8] {
        self.bytes.get(self.offset..).unwrap_or_default()
    }

    fn u8(&mut self) -> Result<u8, ()> {
        self.take(1)?.first().copied().ok_or(())
    }

    fn u16(&mut self) -> Result<u16, ()> {
        self.take(2)?
            .try_into()
            .map(u16::from_le_bytes)
            .map_err(|_| ())
    }

    fn u32(&mut self) -> Result<u32, ()> {
        self.take(4)?
            .try_into()
            .map(u32::from_le_bytes)
            .map_err(|_| ())
    }

    fn i32(&mut self) -> Result<i32, ()> {
        self.take(4)?
            .try_into()
            .map(i32::from_le_bytes)
            .map_err(|_| ())
    }

    fn u64(&mut self) -> Result<u64, ()> {
        self.take(8)?
            .try_into()
            .map(u64::from_le_bytes)
            .map_err(|_| ())
    }
}

fn rgb(value: u32) -> Color {
    let channels = rgb_channels(value);
    Color::rgba(channels[0], channels[1], channels[2], 1.0)
}

fn rgb_channels(value: u32) -> [f32; 3] {
    [
        ((value >> 16) & 0xff) as f32 / 255.0,
        ((value >> 8) & 0xff) as f32 / 255.0,
        (value & 0xff) as f32 / 255.0,
    ]
}

bindings::export!(TerminalUi with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::{
        Command, DEFAULT_BG, FOCUS_MESSAGE, MAX_PRESENTATION_BATCH_COMMANDS, SNAPSHOT_FULL,
        SNAPSHOT_MAGIC, SNAPSHOT_VERSION, STATE, UiState, decode_chunk,
        finish_cursor_blink_for_state, focus_commands_for_mailbox, paste_bytes, row_cells,
        terminal_focuses_on_create, terminal_ready_commands, terminal_session_actor,
        terminal_text_clusters,
    };
    use brood_graphics::theme::ashen;

    fn one_row_snapshot(cols: u16, run_cells: u16, text: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SNAPSHOT_MAGIC);
        bytes.push(SNAPSHOT_VERSION);
        bytes.push(SNAPSHOT_FULL);
        bytes.extend_from_slice(&7_u64.to_le_bytes());
        bytes.extend_from_slice(&4_u64.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&cols.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.push(1);
        bytes.push(0);
        bytes.push(0);
        bytes.extend_from_slice(&i32::MIN.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        bytes.extend_from_slice(&32_u64.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&run_cells.to_le_bytes());
        bytes.extend_from_slice(&ashen::TEXT_RGB.to_le_bytes());
        bytes.extend_from_slice(&DEFAULT_BG.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&(text.len() as u16).to_le_bytes());
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    #[test]
    fn decoder_accepts_resized_history_with_wide_combining_text() {
        let bytes = one_row_snapshot(3, 3, "界\u{301} ");
        let (header, chunk_index, chunk_count, row_start, rows) =
            decode_chunk(&bytes).expect("terminal-session compatible snapshot");
        assert_eq!((header.rows, header.cols), (1, 3));
        assert_eq!((header.viewport_offset, header.max_viewport_offset), (1, 2));
        assert_eq!((chunk_index, chunk_count, row_start), (0, 1, 0));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 1);
        assert_eq!(rows[0][0].cells, 3);
        assert_eq!(rows[0][0].text, "界\u{301} ");
        assert_eq!(row_cells(header.cols, &rows[0]), vec!["界\u{301}", "", " "]);

        let invalid = one_row_snapshot(3, 4, "broken");
        assert!(decode_chunk(&invalid).is_err());
    }

    #[test]
    fn opted_in_presentation_batch_stays_within_host_command_capacity() {
        assert_eq!(MAX_PRESENTATION_BATCH_COMMANDS, 64);
    }

    #[test]
    fn backend_scoped_default_ui_targets_default_terminal() {
        assert_eq!(
            terminal_session_actor("terminal-ui-b2-s7"),
            "terminal-session"
        );
    }

    #[test]
    fn backend_scoped_dynamic_ui_retains_terminal_suffix() {
        assert_eq!(
            terminal_session_actor("terminal-ui-b2-s7-4"),
            "terminal-session-4"
        );
    }

    #[test]
    fn default_terminal_creation_does_not_steal_focus_but_dynamic_creation_does() {
        assert!(!terminal_focuses_on_create("terminal-ui-b2-s7"));
        let mut default = UiState::new(2, 7, "terminal-ui-b2-s7");
        default.pane = 41;
        let default_commands = terminal_ready_commands(&mut default);
        assert!(
            !default_commands
                .iter()
                .any(|command| matches!(command, Command::SendHostMessage(_)))
        );
        assert!(matches!(
            default_commands.first(),
            Some(Command::SendSessionMessage(message)) if message.body == b"A"
        ));

        assert!(terminal_focuses_on_create("terminal-ui-b2-s7-4"));
        let mut dynamic = UiState::new(2, 7, "terminal-ui-b2-s7-4");
        dynamic.pane = 42;
        let dynamic_commands = terminal_ready_commands(&mut dynamic);
        assert!(matches!(
            dynamic_commands.first(),
            Some(Command::SendHostMessage(body))
                if body == br#"{"type":"focus-pane","pane":42}"#
        ));
    }

    #[test]
    fn every_neovim_ui_id_targets_the_editor_session() {
        assert_eq!(terminal_session_actor("neovim-ui"), "neovim-session");
        assert_eq!(terminal_session_actor("neovim-ui-b2-s7"), "neovim-session");
    }

    #[test]
    fn trusted_client_chrome_can_focus_an_existing_terminal_pane() {
        *STATE.lock().expect("terminal UI state") = Some(UiState::new(2, 7, "neovim-ui-b2-s7"));
        STATE
            .lock()
            .expect("terminal UI state")
            .as_mut()
            .unwrap()
            .pane = 42;
        let commands = focus_commands_for_mailbox("brood.client-init", FOCUS_MESSAGE);
        assert!(matches!(
            commands.first(),
            Some(Command::SendHostMessage(body))
                if body == br#"{"type":"focus-pane","pane":42}"#
        ));
        assert!(
            matches!(commands.get(1), Some(Command::SendSessionMessage(message)) if message.body == b"A")
        );
        assert!(
            matches!(commands.get(2), Some(Command::SendSessionMessage(message)) if message.body.starts_with(b"R"))
        );
        assert_eq!(focus_commands_for_mailbox("inbox", FOCUS_MESSAGE).len(), 3);
        assert!(focus_commands_for_mailbox("untrusted", FOCUS_MESSAGE).is_empty());
        STATE
            .lock()
            .expect("terminal UI state")
            .as_mut()
            .unwrap()
            .pane = 0;
        assert!(focus_commands_for_mailbox("brood.client-init", FOCUS_MESSAGE).is_empty());
    }

    #[test]
    fn bracketed_paste_has_exactly_one_delimiter_pair() {
        assert_eq!(
            paste_bytes("one\r\ntwo", true),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
    }

    #[test]
    fn cursor_row_clusters_reuse_glyphs_without_laying_out_padding() {
        assert_eq!(
            terminal_text_clusters("a界e\u{301}  "),
            vec![
                (0, 1, "a".to_string()),
                (1, 2, "界".to_string()),
                (3, 1, "e\u{301}".to_string()),
            ]
        );
    }

    #[test]
    fn cursor_blink_worker_is_single_flight_and_stops_while_unfocused() {
        let mut state = UiState::new(2, 7, "terminal-ui-b2-s7");
        assert!(state.schedule_cursor_blink().is_none());
        state.focused = true;
        assert!(matches!(
            state.schedule_cursor_blink(),
            Some(Command::SpawnWorker(_))
        ));
        assert!(state.schedule_cursor_blink().is_none());
        state.cursor_blink_worker = None;
        state.focused = false;
        assert!(state.schedule_cursor_blink().is_none());
    }

    #[test]
    fn cursor_input_reuses_the_bounded_blink_deadline() {
        let mut state = UiState::new(2, 7, "terminal-ui-b2-s7");
        state.focused = true;
        let Some(Command::SpawnWorker(first)) = state.schedule_cursor_blink() else {
            panic!("focused cursor did not schedule its first blink deadline");
        };
        state.cursor_blink_on = true;

        assert!(state.restart_cursor_blink().is_empty());
        assert_eq!(state.cursor_blink_worker, Some(first.worker));

        let (redraw, commands) = finish_cursor_blink_for_state(&mut state, first.worker);
        assert!(redraw);
        assert!(matches!(commands.as_slice(), [Command::SpawnWorker(_)]));
        assert!(!state.cursor_blink_on);
    }
}
