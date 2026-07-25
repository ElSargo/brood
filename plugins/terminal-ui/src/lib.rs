wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use brood::plugin::host;
use brood_graphics::{Color, Command as DrawCommand, Packet, Rect};
use exports::brood::plugin::actor::{
    Command, Event, Guest, LifecycleEvent, PresentationEvent, SessionActorMessage, WorkerEvent,
    WorkerFinished, WorkerRequest,
};

const SNAPSHOT_MAGIC: &[u8; 4] = b"BTRM";
const FONT_SIZE: f32 = 16.0;
const REQUESTED_LINE_HEIGHT: f32 = 20.0;
const PADDING: f32 = 8.0;
const DEFAULT_BG: u32 = 0x1d2021;
const CURSOR: u32 = 0x928374;
const MOD_CTRL: u32 = 1;
const MOD_ALT: u32 = 2;
const MOD_SHIFT: u32 = 4;

static STATE: Mutex<Option<UiState>> = Mutex::new(None);

struct TerminalUi;

struct UiState {
    pane: u64,
    session_id: u64,
    session_actor: String,
    width: f32,
    height: f32,
    cell_width: f32,
    line_height: f32,
    assembly: Option<SnapshotAssembly>,
    latest: Option<Snapshot>,
    render_active: bool,
    render_worker: Option<u64>,
    next_worker: u64,
    rendered_revision: u64,
    cache: BTreeMap<RunKey, u64>,
    layouts_created: u64,
    layouts_reused: u64,
    layouts_dropped: u64,
    packets: u64,
    max_packet_bytes: usize,
    max_commands: usize,
    requested_viewport: usize,
}

impl UiState {
    fn new(session_id: u64, instance_id: &str) -> Self {
        let session_actor = instance_id.strip_prefix("terminal-ui-").map_or_else(
            || "terminal-session".to_string(),
            |suffix| format!("terminal-session-{suffix}"),
        );
        Self {
            pane: 0,
            session_id,
            session_actor,
            width: 640.0,
            height: 480.0,
            cell_width: 9.0,
            line_height: REQUESTED_LINE_HEIGHT,
            assembly: None,
            latest: None,
            render_active: false,
            render_worker: None,
            next_worker: 1,
            rendered_revision: 0,
            cache: BTreeMap::new(),
            layouts_created: 0,
            layouts_reused: 0,
            layouts_dropped: 0,
            packets: 0,
            max_packet_bytes: 0,
            max_commands: 0,
            requested_viewport: 0,
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
            session_id: self.session_id,
            actor: self.session_actor.clone(),
            body,
        })
    }

    fn resize_command(&self) -> Command {
        let (rows, cols) = self.grid_size();
        let mut body = vec![b'R'];
        body.extend_from_slice(&rows.to_le_bytes());
        body.extend_from_slice(&cols.to_le_bytes());
        self.session_message(body)
    }

    fn viewport_command(&self, offset: usize) -> Command {
        let mut body = vec![b'V'];
        body.extend_from_slice(&u32::try_from(offset).unwrap_or(u32::MAX).to_le_bytes());
        self.session_message(body)
    }

    fn start_render_if_idle(&mut self) -> Vec<Command> {
        if self.render_active
            || self
                .latest
                .as_ref()
                .is_none_or(|snapshot| snapshot.revision <= self.rendered_revision)
        {
            return Vec::new();
        }
        self.render_active = true;
        let worker = self.next_worker;
        self.next_worker = self.next_worker.saturating_add(1).max(1);
        self.render_worker = Some(worker);
        vec![Command::SpawnWorker(WorkerRequest {
            worker,
            payload: Vec::new(),
        })]
    }
}

#[derive(Clone)]
struct Snapshot {
    revision: u64,
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    cursor_visible: bool,
    application_cursor: bool,
    bracketed_paste: bool,
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
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    cursor_visible: bool,
    application_cursor: bool,
    bracketed_paste: bool,
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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RunKey {
    cells: u16,
    fg: u32,
    bold: bool,
    faint: bool,
    italic: bool,
    text: String,
}

impl Guest for TerminalUi {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(context)) => {
                let Some(session_id) = context.session_id else {
                    return vec![Command::LifecycleComplete(Err(
                        "terminal UI has no bound session".to_string(),
                    ))];
                };
                if let Ok(mut state) = STATE.lock() {
                    *state = Some(UiState::new(session_id, &context.instance_id));
                }
                vec![Command::LifecycleComplete(Ok(()))]
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![Command::LifecycleComplete(Ok(()))]
            }
            Event::Presentation(event) => handle_presentation(event),
            Event::Mailbox(message)
                if message.sender.starts_with("session:")
                    && message.body.starts_with(SNAPSHOT_MAGIC) =>
            {
                handle_snapshot_chunk(&message.body)
            }
            Event::Worker(WorkerEvent::Finished(WorkerFinished { worker, output })) => {
                finish_render(worker, &output)
            }
            Event::Worker(WorkerEvent::Failed(failed)) => {
                let Ok(mut guard) = STATE.lock() else {
                    return Vec::new();
                };
                let Some(state) = guard.as_mut() else {
                    return Vec::new();
                };
                if state.render_worker != Some(failed.worker) {
                    return Vec::new();
                }
                state.render_active = false;
                state.render_worker = None;
                state.start_render_if_idle()
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        render_latest_snapshot()
    }
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
            vec![
                Command::SendHostMessage(
                    format!(r#"{{"type":"focus-pane","pane":{}}}"#, state.pane).into_bytes(),
                ),
                state.session_message(vec![b'A']),
                state.resize_command(),
            ]
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            state.width = resized.width.max(1.0);
            state.height = resized.height.max(1.0);
            vec![state.resize_command()]
        }
        PresentationEvent::Key(key) if key.pane == state.pane && key.pressed => {
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
                return vec![state.viewport_command(viewport)];
            }
            let application_cursor = state
                .latest
                .as_ref()
                .is_some_and(|snapshot| snapshot.application_cursor);
            key_bytes(&key.logical_key, key.modifiers, application_cursor)
                .map(|bytes| {
                    state.requested_viewport = 0;
                    state.session_message(input_body(&bytes))
                })
                .into_iter()
                .collect()
        }
        PresentationEvent::Text(text) if text.pane == state.pane && !text.text.is_empty() => {
            let printable = text
                .text
                .chars()
                .filter(|character| !character.is_control())
                .collect::<String>();
            if printable.is_empty() {
                Vec::new()
            } else {
                state.requested_viewport = 0;
                vec![state.session_message(input_body(printable.as_bytes()))]
            }
        }
        PresentationEvent::PointerScroll(scroll) if scroll.pane == state.pane => {
            let Some(snapshot) = state.latest.as_ref() else {
                return Vec::new();
            };
            let delta = if scroll.vertical > 0.0 {
                3_isize
            } else if scroll.vertical < 0.0 {
                -3
            } else {
                0
            };
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
        _ => Vec::new(),
    }
}

fn input_body(bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 1);
    body.push(b'I');
    body.extend_from_slice(bytes);
    body
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
    if state
        .latest
        .as_ref()
        .is_some_and(|snapshot| snapshot.revision > header.revision)
    {
        return Vec::new();
    }
    if state
        .assembly
        .as_ref()
        .is_none_or(|assembly| assembly.header.revision != header.revision)
    {
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
    let mut rows_data = vec![Vec::new(); usize::from(header.rows)];
    for chunk in assembly.chunks.iter().flatten() {
        for (row, runs) in chunk {
            if let Some(target) = rows_data.get_mut(usize::from(*row)) {
                *target = runs.clone();
            }
        }
    }
    state.latest = Some(Snapshot {
        revision: header.revision,
        rows: header.rows,
        cols: header.cols,
        cursor_row: header.cursor_row,
        cursor_col: header.cursor_col,
        cursor_visible: header.cursor_visible,
        application_cursor: header.application_cursor,
        bracketed_paste: header.bracketed_paste,
        exited: header.exited,
        exit_code: header.exit_code,
        chunks_read: header.chunks_read,
        bytes_read: header.bytes_read,
        resizes: header.resizes,
        viewport_offset: header.viewport_offset,
        max_viewport_offset: header.max_viewport_offset,
        rows_data,
    });
    state.requested_viewport = header.viewport_offset;
    state.assembly = None;
    state.start_render_if_idle()
}

fn render_latest_snapshot() -> Result<Vec<u8>, String> {
    let (pane, width, height, cell_width, line_height, snapshot, mut cache) = {
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
            std::mem::take(&mut state.cache),
        )
    };

    let mut packet = Packet::new();
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, 0.0, width, height),
        color: rgb(DEFAULT_BG),
    });
    let mut active = BTreeSet::new();
    let mut created = 0_u64;
    let mut reused = 0_u64;
    for (row, runs) in snapshot.rows_data.iter().enumerate() {
        let y = PADDING + row as f32 * line_height;
        for run in runs {
            let x = PADDING + f32::from(run.start) * cell_width;
            let run_width = f32::from(run.cells) * cell_width;
            if run.bg != DEFAULT_BG {
                packet.push(DrawCommand::FillRect {
                    rect: Rect::new(x, y, run_width, line_height),
                    color: rgb(run.bg),
                });
            }
            if run.text.trim_end().is_empty() {
                continue;
            }
            let key = RunKey {
                cells: run.cells,
                fg: run.fg,
                bold: run.bold,
                faint: run.faint,
                italic: run.italic,
                text: run.text.clone(),
            };
            active.insert(key.clone());
            let handle = if let Some(handle) = cache.get(&key) {
                reused = reused.saturating_add(1);
                *handle
            } else {
                let color = rgb_channels(run.fg);
                let layout = host::graphics_layout_text(
                    pane,
                    &run.text,
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
                        max_width: run_width.max(cell_width),
                        line_height,
                        red: color[0],
                        green: color[1],
                        blue: color[2],
                        alpha: if run.faint { 0.6 } else { 1.0 },
                    },
                )
                .map_err(|error| format!("layout terminal run: {error:?}"))?;
                cache.insert(key.clone(), layout.handle);
                created = created.saturating_add(1);
                layout.handle
            };
            packet.push(DrawCommand::DrawTextLayout {
                layout: handle,
                x,
                y,
            });
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
            color: rgb(0xcc241d),
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
    host::graphics_submit_draw_packet(pane, &bytes)
        .map_err(|error| format!("submit terminal draw packet: {error:?}"))?;
    let obsolete = cache
        .keys()
        .filter(|key| !active.contains(*key))
        .cloned()
        .collect::<Vec<_>>();
    let mut dropped = 0_u64;
    for key in obsolete {
        if let Some(handle) = cache.remove(&key) {
            host::graphics_drop_resource(pane, handle)
                .map_err(|error| format!("drop obsolete terminal layout: {error:?}"))?;
            dropped = dropped.saturating_add(1);
        }
    }

    {
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
        state.max_packet_bytes = state.max_packet_bytes.max(bytes.len());
        state.max_commands = state.max_commands.max(command_count);
    }
    Ok(format!(
        "{{\"revision\":{},\"rows\":{},\"cols\":{},\"packet_bytes\":{},\"commands\":{},\"layouts_created\":{},\"layouts_reused\":{},\"layouts_dropped\":{},\"pty_chunks\":{},\"pty_bytes\":{},\"pty_resizes\":{},\"viewport_offset\":{},\"max_viewport_offset\":{},\"application_cursor\":{},\"bracketed_paste\":{},\"exited\":{},\"exit_code\":{}}}",
        snapshot.revision,
        snapshot.rows,
        snapshot.cols,
        bytes.len(),
        command_count,
        created,
        reused,
        dropped,
        snapshot.chunks_read,
        snapshot.bytes_read,
        snapshot.resizes,
        snapshot.viewport_offset,
        snapshot.max_viewport_offset,
        snapshot.application_cursor,
        snapshot.bracketed_paste,
        snapshot.exited,
        snapshot
            .exit_code
            .map_or_else(|| "null".to_string(), |code| code.to_string()),
    )
    .into_bytes())
}

fn finish_render(worker: u64, output: &[u8]) -> Vec<Command> {
    let revision = parse_metric_u64(output, b"\"revision\":").unwrap_or(0);
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    if state.render_worker != Some(worker) {
        return Vec::new();
    }
    state.render_active = false;
    state.render_worker = None;
    state.rendered_revision = state.rendered_revision.max(revision);
    state.start_render_if_idle()
}

fn parse_metric_u64(bytes: &[u8], prefix: &[u8]) -> Option<u64> {
    let start = bytes
        .windows(prefix.len())
        .position(|window| window == prefix)?
        + prefix.len();
    let digits = bytes
        .get(start..)?
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .copied()
        .collect::<Vec<_>>();
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

type DecodedChunk = (SnapshotHeader, u16, u16, u16, Vec<Vec<Run>>);

fn decode_chunk(bytes: &[u8]) -> Result<DecodedChunk, ()> {
    let mut reader = Reader::new(bytes);
    if reader.take(4)? != SNAPSHOT_MAGIC || reader.u8()? != 2 {
        return Err(());
    }
    let revision = reader.u64()?;
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
            rows,
            cols,
            cursor_row,
            cursor_col,
            cursor_visible,
            application_cursor: screen_flags & 1 != 0,
            bracketed_paste: screen_flags & 2 != 0,
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

export!(TerminalUi);
