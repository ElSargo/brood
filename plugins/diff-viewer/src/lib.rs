wit_bindgen::generate!({
    path: "../../crates/brood-wasm/wit",
    world: "brood-plugin",
});

use std::sync::Mutex;

use brood::plugin::host;
use brood_graphics::{Color, Command, Packet, Rect};
use exports::brood::plugin::actor::{
    Command as ActorCommand, Event, Guest, LifecycleEvent, PresentationEvent, WorkerEvent,
    WorkerRequest,
};

const FIRST_WORKER: u64 = 1;
const HEADER_HEIGHT: f32 = 52.0;
const FOOTER_HEIGHT: f32 = 28.0;
const FILE_HEADER_HEIGHT: f32 = 34.0;
const HUNK_HEADER_HEIGHT: f32 = 28.0;
const CODE_ROW_HEIGHT: f32 = 22.0;
const FILE_GAP_HEIGHT: f32 = 12.0;
const GUTTER_WIDTH: f32 = 54.0;
const MAX_DIFF_BYTES: usize = 900 * 1024;
const MAX_UNTRACKED_FILES: usize = 128;

// Zed One Dark tokens, with diff backgrounds derived from its version-control
// colors at the same subtle opacity used by the editor.
const BG: Color = rgb(0x28, 0x2c, 0x33);
const SURFACE: Color = rgb(0x2f, 0x34, 0x3e);
const ACTIVE: Color = rgb(0x45, 0x4a, 0x56);
const BORDER: Color = rgb(0x46, 0x4b, 0x57);
const FG: Color = rgb(0xac, 0xb2, 0xbe);
const TEXT: Color = rgb(0xdc, 0xe0, 0xe5);
const MUTED: Color = rgb(0x87, 0x8a, 0x98);
const LINE_NUMBER: Color = rgb(0x4e, 0x5a, 0x5f);
const ADDED: Color = rgb(0x27, 0xa6, 0x57);
const DELETED: Color = rgb(0xe0, 0x6c, 0x76);
const MODIFIED: Color = rgb(0xd3, 0xb0, 0x20);
const ADDED_BG: Color = Color::rgba(0.153, 0.651, 0.341, 0.16);
const DELETED_BG: Color = Color::rgba(0.878, 0.424, 0.463, 0.16);
const EMPTY_BG: Color = Color::rgba(0.18, 0.20, 0.24, 0.72);

const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::rgba(
        red as f32 / 255.0,
        green as f32 / 255.0,
        blue as f32 / 255.0,
        1.0,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChangeKind {
    Context,
    Changed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DisplayRow {
    File {
        path: String,
        additions: usize,
        deletions: usize,
    },
    Hunk(String),
    Code {
        old_line: Option<u32>,
        new_line: Option<u32>,
        left: Option<String>,
        right: Option<String>,
        kind: ChangeKind,
    },
    Gap,
}

impl DisplayRow {
    const fn height(&self) -> f32 {
        match self {
            Self::File { .. } => FILE_HEADER_HEIGHT,
            Self::Hunk(_) => HUNK_HEADER_HEIGHT,
            Self::Code { .. } => CODE_ROW_HEIGHT,
            Self::Gap => FILE_GAP_HEIGHT,
        }
    }
}

#[derive(Default)]
struct DiffDocument {
    rows: Vec<DisplayRow>,
    files: usize,
    additions: usize,
    deletions: usize,
    truncated: bool,
}

struct ViewerState {
    pane: u64,
    width: f32,
    height: f32,
    scroll: f32,
    document: DiffDocument,
    loading: bool,
    error: Option<String>,
    next_worker: u64,
    active_worker: Option<u64>,
    text_resources: Vec<u64>,
}

impl ViewerState {
    const fn new() -> Self {
        Self {
            pane: 0,
            width: 960.0,
            height: 720.0,
            scroll: 0.0,
            document: DiffDocument {
                rows: Vec::new(),
                files: 0,
                additions: 0,
                deletions: 0,
                truncated: false,
            },
            loading: false,
            error: None,
            next_worker: FIRST_WORKER,
            active_worker: None,
            text_resources: Vec::new(),
        }
    }

    fn content_height(&self) -> f32 {
        self.document.rows.iter().map(DisplayRow::height).sum()
    }

    fn viewport_height(&self) -> f32 {
        (self.height - HEADER_HEIGHT - FOOTER_HEIGHT).max(1.0)
    }

    fn max_scroll(&self) -> f32 {
        (self.content_height() - self.viewport_height()).max(0.0)
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }

    fn begin_refresh(&mut self) -> Option<ActorCommand> {
        if self.active_worker.is_some() {
            return None;
        }
        let worker = self.next_worker;
        self.next_worker = self.next_worker.saturating_add(1).max(FIRST_WORKER);
        self.active_worker = Some(worker);
        self.loading = true;
        self.error = None;
        Some(ActorCommand::SpawnWorker(WorkerRequest {
            worker,
            payload: Vec::new(),
        }))
    }
}

static STATE: Mutex<ViewerState> = Mutex::new(ViewerState::new());

struct DiffViewer;

impl Guest for DiffViewer {
    fn dispatch(event: Event) -> Vec<ActorCommand> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Presentation(event) => handle_presentation(event),
            Event::Worker(event) => handle_worker(event),
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        collect_project_diff().await
    }
}

fn handle_presentation(event: PresentationEvent) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match event {
        PresentationEvent::PaneCreated(created) => {
            state.pane = created.pane;
            state.width = created.width.max(480.0);
            state.height = created.height.max(320.0);
            state.scroll = 0.0;
            let command = state.begin_refresh();
            redraw_or_record_error(&mut state);
            let mut commands = command.into_iter().collect::<Vec<_>>();
            commands.push(ActorCommand::SendHostMessage(
                format!(r#"{{"type":"focus-pane","pane":{}}}"#, state.pane).into_bytes(),
            ));
            commands
        }
        PresentationEvent::PaneResized(resized) if resized.pane == state.pane => {
            let width = resized.width.max(480.0);
            let height = resized.height.max(320.0);
            if (state.width - width).abs() < 0.5 && (state.height - height).abs() < 0.5 {
                return Vec::new();
            }
            state.width = width;
            state.height = height;
            state.clamp_scroll();
            redraw_or_record_error(&mut state);
            Vec::new()
        }
        PresentationEvent::PointerScroll(scroll) if scroll.pane == state.pane => {
            let delta = if scroll.vertical.abs() < 1.0 {
                scroll.vertical.signum() * 48.0
            } else {
                scroll.vertical * 2.0
            };
            state.scroll = (state.scroll - delta).clamp(0.0, state.max_scroll());
            redraw_or_record_error(&mut state);
            Vec::new()
        }
        PresentationEvent::Key(key) if key.pane == state.pane && key.pressed && !key.repeat => {
            let command = match key.logical_key.as_str() {
                "ArrowDown" | "j" => {
                    state.scroll += CODE_ROW_HEIGHT * 3.0;
                    None
                }
                "ArrowUp" | "k" => {
                    state.scroll -= CODE_ROW_HEIGHT * 3.0;
                    None
                }
                "PageDown" | " " => {
                    state.scroll += state.viewport_height() * 0.82;
                    None
                }
                "PageUp" => {
                    state.scroll -= state.viewport_height() * 0.82;
                    None
                }
                "Home" => {
                    state.scroll = 0.0;
                    None
                }
                "End" => {
                    state.scroll = state.max_scroll();
                    None
                }
                "r" | "R" => state.begin_refresh(),
                _ => return Vec::new(),
            };
            state.clamp_scroll();
            redraw_or_record_error(&mut state);
            command.into_iter().collect()
        }
        _ => Vec::new(),
    }
}

fn handle_worker(event: WorkerEvent) -> Vec<ActorCommand> {
    let Ok(mut state) = STATE.lock() else {
        return Vec::new();
    };
    match event {
        WorkerEvent::Finished(finished) if state.active_worker == Some(finished.worker) => {
            state.active_worker = None;
            state.loading = false;
            let text = String::from_utf8_lossy(&finished.output);
            state.document = parse_diff(&text);
            state.scroll = 0.0;
            state.error = None;
            redraw_or_record_error(&mut state);
        }
        WorkerEvent::Failed(failed) if state.active_worker == Some(failed.worker) => {
            state.active_worker = None;
            state.loading = false;
            state.error = Some(failed.message);
            redraw_or_record_error(&mut state);
        }
        WorkerEvent::Cancelled(worker) if state.active_worker == Some(worker) => {
            state.active_worker = None;
            state.loading = false;
            state.error = Some("Diff refresh was cancelled".to_string());
            redraw_or_record_error(&mut state);
        }
        _ => {}
    }
    Vec::new()
}

fn redraw_or_record_error(state: &mut ViewerState) {
    if state.pane == 0 {
        return;
    }
    if let Err(error) = redraw(state) {
        state.error = Some(error);
    }
}

fn redraw(state: &mut ViewerState) -> Result<(), String> {
    let mut packet = Packet::new();
    let mut resources = Vec::new();
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, state.height),
        color: BG,
    });
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, HEADER_HEIGHT),
        color: SURFACE,
    });
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, HEADER_HEIGHT - 1.0, state.width, 1.0),
        color: BORDER,
    });
    add_text(
        state,
        &mut packet,
        &mut resources,
        "Project Diff",
        17.0,
        TEXT,
        16.0,
        14.0,
        180.0,
    )?;
    let summary = if state.loading {
        "Refreshing…".to_string()
    } else {
        format!(
            "{} files   +{}   −{}",
            state.document.files, state.document.additions, state.document.deletions
        )
    };
    add_text(
        state,
        &mut packet,
        &mut resources,
        &summary,
        13.0,
        if state.loading { MODIFIED } else { MUTED },
        142.0,
        17.0,
        (state.width - 390.0).max(120.0),
    )?;
    add_text(
        state,
        &mut packet,
        &mut resources,
        "R refresh   Ctrl+Alt+D close",
        12.0,
        MUTED,
        (state.width - 220.0).max(260.0),
        18.0,
        204.0,
    )?;

    let content_top = HEADER_HEIGHT;
    let content_bottom = state.height - FOOTER_HEIGHT;
    let split_x = state.width * 0.5;
    packet.push(Command::Save);
    packet.push(Command::ClipRect(Rect::new(
        0.0,
        content_top,
        state.width,
        (content_bottom - content_top).max(1.0),
    )));
    packet.push(Command::FillRect {
        rect: Rect::new(
            split_x - 0.5,
            content_top,
            1.0,
            content_bottom - content_top,
        ),
        color: BORDER,
    });

    let mut y = content_top - state.scroll;
    for row in &state.document.rows {
        let height = row.height();
        if y + height >= content_top && y <= content_bottom {
            draw_row(state, &mut packet, &mut resources, row, y, height, split_x)?;
        }
        y += height;
        if y > content_bottom {
            break;
        }
    }
    packet.push(Command::Restore);

    if state.document.rows.is_empty() && state.error.is_none() {
        add_text(
            state,
            &mut packet,
            &mut resources,
            if state.loading {
                "Loading project changes…"
            } else {
                "Working tree clean"
            },
            19.0,
            if state.loading { MUTED } else { ADDED },
            24.0,
            HEADER_HEIGHT + 26.0,
            state.width - 48.0,
        )?;
        add_text(
            state,
            &mut packet,
            &mut resources,
            if state.loading {
                "Git runs in a worker; the pane remains responsive."
            } else {
                "No tracked or untracked changes to show."
            },
            13.0,
            MUTED,
            24.0,
            HEADER_HEIGHT + 58.0,
            state.width - 48.0,
        )?;
    }
    if let Some(error) = &state.error {
        packet.push(Command::FillRoundedRect {
            rect: Rect::new(18.0, HEADER_HEIGHT + 18.0, state.width - 36.0, 92.0),
            radius: 7.0,
            color: DELETED_BG,
        });
        add_text(
            state,
            &mut packet,
            &mut resources,
            "Could not load project diff",
            17.0,
            DELETED,
            30.0,
            HEADER_HEIGHT + 30.0,
            state.width - 60.0,
        )?;
        add_text(
            state,
            &mut packet,
            &mut resources,
            &one_line(error, 180),
            13.0,
            FG,
            30.0,
            HEADER_HEIGHT + 61.0,
            state.width - 60.0,
        )?;
    }

    draw_scrollbar(state, &mut packet);
    packet.push(Command::FillRect {
        rect: Rect::new(
            0.0,
            state.height - FOOTER_HEIGHT,
            state.width,
            FOOTER_HEIGHT,
        ),
        color: SURFACE,
    });
    let footer = if state.document.truncated {
        "Diff capped at 900 KiB · refine the change set or review from the CLI"
    } else {
        "Scroll or use j/k, Page Up/Down, Home, and End"
    };
    add_text(
        state,
        &mut packet,
        &mut resources,
        footer,
        11.0,
        MUTED,
        14.0,
        state.height - 21.0,
        state.width - 28.0,
    )?;

    let bytes = packet.encode().map_err(|error| error.to_string())?;
    host::graphics_submit_draw_packet(state.pane, &bytes)
        .map_err(|error| format!("submit diff viewer: {error:?}"))?;
    for handle in state.text_resources.drain(..) {
        let _ = host::graphics_drop_resource(state.pane, handle);
    }
    state.text_resources = resources;
    Ok(())
}

fn draw_row(
    state: &ViewerState,
    packet: &mut Packet,
    resources: &mut Vec<u64>,
    row: &DisplayRow,
    y: f32,
    height: f32,
    split_x: f32,
) -> Result<(), String> {
    match row {
        DisplayRow::File {
            path,
            additions,
            deletions,
        } => {
            packet.push(Command::FillRect {
                rect: Rect::new(0.0, y, state.width, height),
                color: SURFACE,
            });
            packet.push(Command::FillRect {
                rect: Rect::new(0.0, y + height - 1.0, state.width, 1.0),
                color: BORDER,
            });
            add_text(
                state,
                packet,
                resources,
                &one_line(path, 160),
                14.0,
                TEXT,
                14.0,
                y + 8.0,
                state.width - 160.0,
            )?;
            add_text(
                state,
                packet,
                resources,
                &format!("+{additions}  −{deletions}"),
                12.0,
                MUTED,
                (state.width - 118.0).max(180.0),
                y + 9.0,
                104.0,
            )?;
        }
        DisplayRow::Hunk(header) => {
            packet.push(Command::FillRect {
                rect: Rect::new(0.0, y, state.width, height),
                color: Color::rgba(ACTIVE.r, ACTIVE.g, ACTIVE.b, 0.58),
            });
            add_text(
                state,
                packet,
                resources,
                &one_line(header, 180),
                12.0,
                MUTED,
                14.0,
                y + 7.0,
                state.width - 28.0,
            )?;
        }
        DisplayRow::Code {
            old_line,
            new_line,
            left,
            right,
            kind,
        } => {
            let left_width = split_x;
            let right_width = state.width - split_x;
            if *kind == ChangeKind::Changed {
                packet.push(Command::FillRect {
                    rect: Rect::new(0.0, y, left_width, height),
                    color: if left.is_some() { DELETED_BG } else { EMPTY_BG },
                });
                packet.push(Command::FillRect {
                    rect: Rect::new(split_x, y, right_width, height),
                    color: if right.is_some() { ADDED_BG } else { EMPTY_BG },
                });
                if left.is_some() {
                    packet.push(Command::FillRect {
                        rect: Rect::new(0.0, y, 3.0, height),
                        color: DELETED,
                    });
                }
                if right.is_some() {
                    packet.push(Command::FillRect {
                        rect: Rect::new(split_x, y, 3.0, height),
                        color: ADDED,
                    });
                }
            }
            packet.push(Command::FillRect {
                rect: Rect::new(GUTTER_WIDTH - 1.0, y, 1.0, height),
                color: Color::rgba(BORDER.r, BORDER.g, BORDER.b, 0.38),
            });
            packet.push(Command::FillRect {
                rect: Rect::new(split_x + GUTTER_WIDTH - 1.0, y, 1.0, height),
                color: Color::rgba(BORDER.r, BORDER.g, BORDER.b, 0.38),
            });
            if let Some(line) = old_line {
                add_text(
                    state,
                    packet,
                    resources,
                    &line.to_string(),
                    11.0,
                    LINE_NUMBER,
                    8.0,
                    y + 5.0,
                    GUTTER_WIDTH - 14.0,
                )?;
            }
            if let Some(line) = new_line {
                add_text(
                    state,
                    packet,
                    resources,
                    &line.to_string(),
                    11.0,
                    LINE_NUMBER,
                    split_x + 8.0,
                    y + 5.0,
                    GUTTER_WIDTH - 14.0,
                )?;
            }
            let left_chars = ((left_width - GUTTER_WIDTH - 16.0) / 7.2).max(8.0) as usize;
            let right_chars = ((right_width - GUTTER_WIDTH - 16.0) / 7.2).max(8.0) as usize;
            if let Some(text) = left {
                add_text(
                    state,
                    packet,
                    resources,
                    &one_line(&expand_tabs(text), left_chars),
                    12.0,
                    FG,
                    GUTTER_WIDTH + 7.0,
                    y + 4.0,
                    left_width - GUTTER_WIDTH - 12.0,
                )?;
            }
            if let Some(text) = right {
                add_text(
                    state,
                    packet,
                    resources,
                    &one_line(&expand_tabs(text), right_chars),
                    12.0,
                    FG,
                    split_x + GUTTER_WIDTH + 7.0,
                    y + 4.0,
                    right_width - GUTTER_WIDTH - 12.0,
                )?;
            }
        }
        DisplayRow::Gap => {}
    }
    Ok(())
}

fn draw_scrollbar(state: &ViewerState, packet: &mut Packet) {
    let content_height = state.content_height();
    let viewport = state.viewport_height();
    if content_height <= viewport {
        return;
    }
    let track_height = viewport - 10.0;
    let thumb_height = (track_height * viewport / content_height).max(28.0);
    let thumb_y = HEADER_HEIGHT
        + 5.0
        + (track_height - thumb_height) * state.scroll / state.max_scroll().max(1.0);
    packet.push(Command::FillRoundedRect {
        rect: Rect::new(state.width - 6.0, thumb_y, 3.0, thumb_height),
        radius: 1.5,
        color: Color::rgba(FG.r, FG.g, FG.b, 0.35),
    });
}

#[allow(clippy::too_many_arguments)]
fn add_text(
    state: &ViewerState,
    packet: &mut Packet,
    resources: &mut Vec<u64>,
    text: &str,
    size: f32,
    color: Color,
    x: f32,
    y: f32,
    max_width: f32,
) -> Result<(), String> {
    let layout = host::graphics_layout_text(
        state.pane,
        text,
        host::TextLayoutOptions {
            font_size: size,
            max_width: max_width.max(1.0),
            line_height: size + 3.0,
            red: color.r,
            green: color.g,
            blue: color.b,
            alpha: color.a,
        },
    )
    .map_err(|error| format!("layout diff text: {error:?}"))?;
    resources.push(layout.handle);
    packet.push(Command::DrawTextLayout {
        layout: layout.handle,
        x,
        y,
    });
    Ok(())
}

fn parse_diff(input: &str) -> DiffDocument {
    let mut document = DiffDocument::default();
    let mut file: Option<FileBuilder> = None;
    let mut hunk: Option<HunkBuilder> = None;

    for line in input.lines() {
        if line == "BROOD_DIFF_TRUNCATED" {
            document.truncated = true;
            continue;
        }
        if let Some(paths) = line.strip_prefix("diff --git ") {
            flush_hunk(&mut hunk, file.as_mut());
            flush_file(&mut file, &mut document);
            let (old_path, new_path) = split_diff_paths(paths);
            file = Some(FileBuilder::new(display_path(&old_path, &new_path)));
            continue;
        }
        let Some(current_file) = file.as_mut() else {
            continue;
        };
        if let Some(header) = line.strip_prefix("@@ ") {
            flush_hunk(&mut hunk, Some(current_file));
            let full_header = format!("@@ {header}");
            if let Some((old_line, new_line)) = parse_hunk_starts(line) {
                hunk = Some(HunkBuilder::new(full_header, old_line, new_line));
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("rename to ") {
            current_file.path = path.to_string();
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ")
            && path != "/dev/null"
        {
            current_file.path = display_path("", path);
            continue;
        }
        if line == "new file mode 100644" || line.starts_with("new file mode ") {
            current_file.new_file = true;
            continue;
        }
        if line == "deleted file mode 100644" || line.starts_with("deleted file mode ") {
            current_file.deleted_file = true;
            continue;
        }
        if line.starts_with("Binary files ") {
            current_file
                .rows
                .push(DisplayRow::Hunk("Binary file changed".to_string()));
            continue;
        }
        if line.starts_with("old mode ") || line.starts_with("new mode ") {
            current_file.rows.push(DisplayRow::Hunk(line.to_string()));
            continue;
        }
        if let Some(current_hunk) = hunk.as_mut() {
            if let Some(text) = line.strip_prefix(' ') {
                current_hunk.flush_changes();
                current_hunk.rows.push(DisplayRow::Code {
                    old_line: Some(current_hunk.old_line),
                    new_line: Some(current_hunk.new_line),
                    left: Some(text.to_string()),
                    right: Some(text.to_string()),
                    kind: ChangeKind::Context,
                });
                current_hunk.old_line += 1;
                current_hunk.new_line += 1;
            } else if let Some(text) = line.strip_prefix('-') {
                current_hunk
                    .removed
                    .push((current_hunk.old_line, text.to_string()));
                current_hunk.old_line += 1;
                current_file.deletions += 1;
            } else if let Some(text) = line.strip_prefix('+') {
                current_hunk
                    .added
                    .push((current_hunk.new_line, text.to_string()));
                current_hunk.new_line += 1;
                current_file.additions += 1;
            } else if line.starts_with("\\ No newline") {
                // The marker annotates the preceding line and is not a visual row.
            } else {
                current_hunk.flush_changes();
            }
        }
    }
    flush_hunk(&mut hunk, file.as_mut());
    flush_file(&mut file, &mut document);
    document
}

struct FileBuilder {
    path: String,
    rows: Vec<DisplayRow>,
    additions: usize,
    deletions: usize,
    new_file: bool,
    deleted_file: bool,
}

impl FileBuilder {
    fn new(path: String) -> Self {
        Self {
            path,
            rows: Vec::new(),
            additions: 0,
            deletions: 0,
            new_file: false,
            deleted_file: false,
        }
    }
}

struct HunkBuilder {
    header: String,
    rows: Vec<DisplayRow>,
    removed: Vec<(u32, String)>,
    added: Vec<(u32, String)>,
    old_line: u32,
    new_line: u32,
}

impl HunkBuilder {
    fn new(header: String, old_line: u32, new_line: u32) -> Self {
        Self {
            header,
            rows: Vec::new(),
            removed: Vec::new(),
            added: Vec::new(),
            old_line,
            new_line,
        }
    }

    fn flush_changes(&mut self) {
        let rows = self.removed.len().max(self.added.len());
        for index in 0..rows {
            let old = self.removed.get(index);
            let new = self.added.get(index);
            self.rows.push(DisplayRow::Code {
                old_line: old.map(|(line, _)| *line),
                new_line: new.map(|(line, _)| *line),
                left: old.map(|(_, text)| text.clone()),
                right: new.map(|(_, text)| text.clone()),
                kind: ChangeKind::Changed,
            });
        }
        self.removed.clear();
        self.added.clear();
    }
}

fn flush_hunk(hunk: &mut Option<HunkBuilder>, file: Option<&mut FileBuilder>) {
    let Some(mut finished) = hunk.take() else {
        return;
    };
    finished.flush_changes();
    if let Some(file) = file {
        file.rows.push(DisplayRow::Hunk(finished.header));
        file.rows.extend(finished.rows);
    }
}

fn flush_file(file: &mut Option<FileBuilder>, document: &mut DiffDocument) {
    let Some(file) = file.take() else {
        return;
    };
    if file.rows.is_empty() {
        return;
    }
    if !document.rows.is_empty() {
        document.rows.push(DisplayRow::Gap);
    }
    let status = if file.new_file {
        "new"
    } else if file.deleted_file {
        "deleted"
    } else {
        ""
    };
    let path = if status.is_empty() {
        file.path
    } else {
        format!("{}  ·  {status}", file.path)
    };
    document.rows.push(DisplayRow::File {
        path,
        additions: file.additions,
        deletions: file.deletions,
    });
    document.rows.extend(file.rows);
    document.files += 1;
    document.additions += file.additions;
    document.deletions += file.deletions;
}

fn parse_hunk_starts(line: &str) -> Option<(u32, u32)> {
    let body = line.strip_prefix("@@ -")?;
    let (old, rest) = body.split_once(' ')?;
    let rest = rest.strip_prefix('+')?;
    let (new, _) = rest.split_once(' ')?;
    Some((parse_range_start(old)?, parse_range_start(new)?))
}

fn parse_range_start(range: &str) -> Option<u32> {
    range.split(',').next()?.parse().ok()
}

fn split_diff_paths(paths: &str) -> (String, String) {
    let mut parts = paths.splitn(2, ' ');
    (
        parts.next().unwrap_or_default().to_string(),
        parts.next().unwrap_or_default().to_string(),
    )
}

fn display_path(old_path: &str, new_path: &str) -> String {
    let preferred = if new_path == "/dev/null" {
        old_path
    } else {
        new_path
    };
    preferred
        .strip_prefix("b/")
        .or_else(|| preferred.strip_prefix("a/"))
        .unwrap_or(preferred)
        .trim_matches('"')
        .to_string()
}

fn expand_tabs(text: &str) -> String {
    text.replace('\t', "    ")
}

fn one_line(text: &str, maximum: usize) -> String {
    let text = text.replace(['\r', '\n'], " ");
    let mut characters = text.chars();
    let mut output = characters.by_ref().take(maximum).collect::<String>();
    if characters.next().is_some() {
        output.push('…');
    }
    output
}

async fn collect_project_diff() -> Result<Vec<u8>, String> {
    let base_args = [
        "diff",
        "--no-ext-diff",
        "--no-color",
        "--find-renames",
        "--unified=3",
        "HEAD",
        "--",
    ];
    let tracked = run_git(&base_args).await?;
    let mut output = Vec::new();
    if tracked.code == 0 {
        append_bounded(&mut output, &tracked.stdout);
    } else if tracked.code == 128 && stderr_mentions_missing_head(&tracked.stderr) {
        let cached = run_git(&[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--unified=3",
            "--cached",
            "--",
        ])
        .await?;
        ensure_git_success(&cached, "read staged changes")?;
        append_bounded(&mut output, &cached.stdout);
        let unstaged = run_git(&[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--unified=3",
            "--",
        ])
        .await?;
        ensure_git_success(&unstaged, "read unstaged changes")?;
        append_bounded(&mut output, &unstaged.stdout);
    } else {
        return Err(process_failure("read tracked changes", &tracked));
    }

    let status = run_git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"]).await?;
    ensure_git_success(&status, "list untracked files")?;
    for path in untracked_paths(&status.stdout)
        .into_iter()
        .take(MAX_UNTRACKED_FILES)
    {
        if output.len() >= MAX_DIFF_BYTES {
            break;
        }
        if path.contains('\n') || path.contains('\r') {
            continue;
        }
        let remaining = MAX_DIFF_BYTES.saturating_sub(output.len());
        let bytes = match host::worker_read_file(path.clone(), 0, remaining as u64).await {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        append_untracked_diff(&mut output, &path, &bytes);
    }
    if output.len() >= MAX_DIFF_BYTES {
        output.truncate(MAX_DIFF_BYTES);
        while output.last().is_some_and(|byte| *byte != b'\n') {
            output.pop();
        }
        output.extend_from_slice(b"BROOD_DIFF_TRUNCATED\n");
    }
    Ok(output)
}

fn append_untracked_diff(output: &mut Vec<u8>, path: &str, bytes: &[u8]) {
    let header = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n"
    );
    append_bounded(output, header.as_bytes());
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        append_bounded(
            output,
            format!("Binary files /dev/null and b/{path} differ\n").as_bytes(),
        );
        return;
    }
    let text = String::from_utf8_lossy(bytes);
    let lines = if text.is_empty() {
        0
    } else {
        text.lines().count()
    };
    append_bounded(output, format!("@@ -0,0 +1,{lines} @@\n").as_bytes());
    for line in text.split_inclusive('\n') {
        append_bounded(output, b"+");
        append_bounded(output, line.as_bytes());
        if !line.ends_with('\n') {
            append_bounded(output, b"\n\\ No newline at end of file\n");
        }
        if output.len() >= MAX_DIFF_BYTES {
            break;
        }
    }
}

struct ProcessOutput {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_git(arguments: &[&str]) -> Result<ProcessOutput, String> {
    let arguments = arguments
        .iter()
        .map(|argument| (*argument).to_string())
        .collect::<Vec<_>>();
    let process = host::subprocess_spawn("git", &arguments)
        .map_err(|error| format!("spawn git: {error:?}"))?;
    let _ = host::stream_close_write(process.stdin);
    let result = drain_process(&process).await;
    let _ = host::stream_drop(process.stdin).await;
    let _ = host::stream_drop(process.stdout).await;
    let _ = host::stream_drop(process.stderr).await;
    let _ = host::child_drop(process.child).await;
    result
}

async fn drain_process(process: &host::ProcessIo) -> Result<ProcessOutput, String> {
    let stdout_notification =
        host::stream_notification(process.stdout).map_err(runtime_error("stdout notification"))?;
    let stderr_notification =
        host::stream_notification(process.stderr).map_err(runtime_error("stderr notification"))?;
    let child_notification =
        host::child_notification(process.child).map_err(runtime_error("child notification"))?;
    let mut stdout_generation = 0;
    let mut stderr_generation = 0;
    let mut child_generation = 0;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        stdout_eof |= drain_stream(process.stdout, &mut stdout)?;
        stderr_eof |= drain_stream(process.stderr, &mut stderr)?;
        let status = host::child_status(process.child).map_err(runtime_error("git status"))?;
        if status.0 != host::ChildState::Running && stdout_eof && stderr_eof {
            return Ok(ProcessOutput {
                code: status.1.unwrap_or(-1),
                stdout,
                stderr,
            });
        }
        let stdout_pollable = host::notification_pollable(stdout_notification, stdout_generation)
            .map_err(runtime_error("stdout pollable"))?;
        let stderr_pollable = host::notification_pollable(stderr_notification, stderr_generation)
            .map_err(runtime_error("stderr pollable"))?;
        let child_pollable = host::notification_pollable(child_notification, child_generation)
            .map_err(runtime_error("child pollable"))?;
        let ready = host::worker_wait_any(
            vec![stdout_pollable, stderr_pollable, child_pollable],
            None,
            host::WaitMode::Fair,
        )
        .await;
        let _ = host::pollable_drop(stdout_pollable);
        let _ = host::pollable_drop(stderr_pollable);
        let _ = host::pollable_drop(child_pollable);
        match ready {
            Ok(ready) if ready.index == 0 => stdout_generation = ready.generation,
            Ok(ready) if ready.index == 1 => stderr_generation = ready.generation,
            Ok(ready) => child_generation = ready.generation,
            Err(host::WaitError::Cancelled) => return Err("git refresh cancelled".to_string()),
            Err(error) => return Err(format!("wait for git: {error:?}")),
        }
    }
}

fn drain_stream(stream: u64, output: &mut Vec<u8>) -> Result<bool, String> {
    loop {
        match host::stream_read(stream, 16 * 1024).map_err(runtime_error("read git stream"))? {
            host::StreamReadResult::Data(bytes) => {
                if output.len() < MAX_DIFF_BYTES {
                    let remaining = MAX_DIFF_BYTES - output.len();
                    output.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
                }
            }
            host::StreamReadResult::WouldBlock => return Ok(false),
            host::StreamReadResult::Eof => return Ok(true),
        }
    }
}

fn append_bounded(output: &mut Vec<u8>, bytes: &[u8]) {
    if output.len() >= MAX_DIFF_BYTES {
        return;
    }
    let remaining = MAX_DIFF_BYTES - output.len();
    output.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
}

fn untracked_paths(status: &[u8]) -> Vec<String> {
    status
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            entry
                .strip_prefix(b"?? ")
                .and_then(|path| String::from_utf8(path.to_vec()).ok())
        })
        .collect()
}

fn stderr_mentions_missing_head(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr);
    stderr.contains("ambiguous argument 'HEAD'")
        || stderr.contains("unknown revision")
        || stderr.contains("bad revision")
}

fn ensure_git_success(output: &ProcessOutput, operation: &str) -> Result<(), String> {
    if output.code == 0 {
        Ok(())
    } else {
        Err(process_failure(operation, output))
    }
}

fn process_failure(operation: &str, output: &ProcessOutput) -> String {
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = one_line(detail.trim(), 240);
    if detail.is_empty() {
        format!("{operation}: git exited with status {}", output.code)
    } else {
        format!("{operation}: {detail}")
    }
}

fn runtime_error<T: core::fmt::Debug>(context: &'static str) -> impl FnOnce(T) -> String {
    move |error| format!("{context}: {error:?}")
}

export!(DiffViewer with_types_in self);

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@
 fn main() {
-    old();
+    new();
+    more();
     done();
 }
diff --git a/old.txt b/new.txt
similarity index 90%
rename from old.txt
rename to new.txt
@@ -2 +2 @@
-before
+after
";

    #[test]
    fn parses_multiple_files_and_aligns_change_blocks() {
        let document = parse_diff(SAMPLE);
        assert_eq!(document.files, 2);
        assert_eq!(document.additions, 3);
        assert_eq!(document.deletions, 2);
        assert!(document.rows.iter().any(|row| {
            matches!(
                row,
                DisplayRow::Code {
                    old_line: None,
                    new_line: Some(3),
                    left: None,
                    right: Some(text),
                    kind: ChangeKind::Changed,
                } if text.trim() == "more();"
            )
        }));
        assert!(
            document
                .rows
                .iter()
                .any(|row| matches!(row, DisplayRow::File { path, .. } if path == "new.txt"))
        );
    }

    #[test]
    fn parses_hunk_ranges_without_counts() {
        assert_eq!(parse_hunk_starts("@@ -12 +4 @@"), Some((12, 4)));
        assert_eq!(parse_hunk_starts("@@ -12,3 +4,8 @@ label"), Some((12, 4)));
    }

    #[test]
    fn extracts_only_utf8_untracked_paths() {
        assert_eq!(
            untracked_paths(b" M tracked\0?? alpha.txt\0?? dir/beta.rs\0"),
            vec!["alpha.txt", "dir/beta.rs"]
        );
    }

    #[test]
    fn synthesizes_parseable_untracked_text_and_binary_diffs() {
        let mut output = Vec::new();
        append_untracked_diff(&mut output, "new file.txt", b"alpha\nbeta\n");
        append_untracked_diff(&mut output, "image.bin", b"\0png");
        let document = parse_diff(&String::from_utf8(output).unwrap());
        assert_eq!(document.files, 2);
        assert_eq!(document.additions, 2);
        assert!(document.rows.iter().any(
            |row| matches!(row, DisplayRow::File { path, .. } if path.starts_with("new file.txt"))
        ));
        assert!(document.rows.iter().any(
            |row| matches!(row, DisplayRow::Hunk(message) if message == "Binary file changed")
        ));
    }
}
